# A volume and brightness OSD

A pill near the bottom of the screen that shows the volume or the
screen brightness when either changes, and fades away a moment after
you stop. It never shows when you log in or save a file. The guide's
[level 4](../../docs/guide/level-4.md#an-osd-for-volume-and-brightness)
explains each line.

## The file

```strand file=osd.strand
// osd.strand. Media keys stay in the compositor: strand set audio.sink.volume +5%
enum Kind { volume, brightness }
state kind = volume
state shown = false
on change audio.sink.volume, audio.sink.muted { kind = volume; shown = true }
on change brightness.level                    { kind = brightness; shown = true }
on change audio.sink.volume, audio.sink.muted, brightness.level after 1.2s { shown = false }
let level = kind == volume ? (audio.sink.muted ? 0 : audio.sink.volume) : brightness.level

osd Level {                                       // focused monitor, overlay, click-through
  anchor: bottom; margin: 0, 0, 96; open: shown
  enter { opacity: 0; scale: 0.9 }
  row { width: 260; pad: $space.3; gap: $space.3; align: center
        radius: full; bg: $surface.alpha(0.8); blur: 24; shadow: $elevation.lg
    icon kind == volume ? audio.sink.icon : "display-brightness-symbolic" { size: 20 }
    meter level { grow: 1; height: 6; color: $accent; track: $fg.alpha(0.15) }
    text pct(level) { width: 4ch; align: end; font: $font.mono }
  }
}
```

## Bind the keys

Strand does not grab keys: your compositor keeps them and runs
`strand set`. These are the commands for the volume and brightness
keys:

```sh
strand set audio.sink.volume +5%
strand set audio.sink.volume -5%
strand set brightness.level +5%
strand set brightness.level -5%
```

Bind them to the keys in your compositor, so they work while the
screen is locked and repeat while held (how: [Start it and bind
keys](../start/running.md#start-it-and-bind-keys)). On a service
field, a signed number or percentage is a step from the value it has
now, and it never goes below 0.

## What it looks like

The volume steps up three times, a few frames apart, as a held key
does. The OSD springs in at the first change, follows the level, and
fades out 1.2 seconds after the last one.

```example osd
```

## How it works

- **`osd Level { … }`** is on the focused monitor, above everything,
  and clicks pass through it to what is underneath. It shows while
  `open:` is true.
- **`on change a, b { … }`** runs when a watched value changes, from
  anywhere: your keys, a slider, another program. It never runs for
  the first value, nor for a value a reload writes, nor when
  `audio.sink` becomes another device because the default output
  changed: the new device's volume is only the new baseline. That is
  why the OSD stays hidden when the shell starts, when you save a file
  and when the default output changes.
- **`on change … after 1.2s`** waits for quiet: it runs once, 1.2
  seconds after the last change, so holding a key keeps the OSD up.
- **`let level`** follows whichever value changed last, and 0 while
  the output is muted.
- **`meter level`** fills from 0 to 1, and `text pct(level)` shows it
  as a percentage in a fixed-width font, `4ch` wide, so the digits do
  not jump.

## Variations

- **Several values in one place.** Past two choices, a `match` reads
  better than nested `?` and `:`; the guide shows the same OSD
  [with `match`](../../docs/guide/level-4.md#the-same-with-match).
- **Rolling digits.** `roll: true` on the `text` rolls the digits when
  the number changes.
- **On every monitor.** `screens: all` on the `osd` puts a copy on each
  monitor, with `screen` in scope in each.

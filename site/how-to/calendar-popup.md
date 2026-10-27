# A bar with a calendar popup

A clock in the middle of the bar that opens a month calendar when you
click it. Escape, or a click anywhere else, closes it. It is the
clock of the guide's bar, with one `when` added to light it while
the calendar shows; the guide's
[level 3](../../docs/guide/level-3.md#a-clock-with-a-calendar) builds
it step by step.

## The files

Put both files in your config directory, `~/.config/strand`.

```strand file=bar.strand
// bar.strand: a bar on every monitor with the clock in the middle
bar Top {
  edge: top; height: 36
  bg: $surface; font: $font.ui; color: $fg
  split { pad: 0, $space.3
    center { Clock }
  }
}
```

```strand file=clock.strand
component Clock {
  state open = false                               // one per monitor
  text clock.format("%a %d  %H:%M") {              // wakes once a minute
    when cal.open { color: $accent }               // lit while the calendar shows
    on click { open = !open }
    popup { id: cal; open: <-> open; Calendar }    // anchored here; Esc or click-away closes
  }
}

component Calendar {
  state month = clock.today.month_start()
  col { pad: $space.3; gap: $space.2; bg: $surface; radius: $radius.lg; shadow: $elevation.lg
    row { align: center
      button "‹" { on click { month = month.add(months: -1) } }
      text month.format("%B %Y") { grow: 1; align: center }
      button "›" { on click { month = month.add(months: 1) } }
    }
    grid { columns: 7; gap: $space.1
      for d in calendar.days(month) {
        text d.date.format("%e") { width: 28; height: 28; align: center
          when !d.in_month { color: $fg.faint }
          when d.date == clock.today { bg: $accent; color: $on_accent; radius: full }
        }
      }
    }
  }
}
```

Save them while `strand run` is running and the bar appears. Nothing
restarts.

## How it works

- **The popup's state lives on `Clock`.** `state open = false` is
  declared in the component, so each monitor's bar has its own `open`:
  opening the calendar on one monitor leaves the others closed.
- **`open: <-> open` is a two-way binding.** The popup shows while
  `open` is true. When Escape or a click away closes it, the popup
  writes `false` back, so the next click on the clock opens it again.
- **A `popup` is anchored to the element it is written in**, here the
  clock's text. On a bar it opens away from the bar's edge: below the
  clock on a top bar.
- **`cal.open`** is true while the popup named `cal` is open. The
  `when` on the text uses it to colour the clock while its calendar
  shows.
- **`clock.format("%a %d  %H:%M")`** shows minutes, so it wakes the
  shell once a minute.
- **`calendar.days(month)`** gives the days of the weeks that cover
  `month`, padded with days of the months around it; `d.in_month` tells
  them apart. `month.add(months: -1)` moves a month back.

## Variations

- **A popup that touches the bar.** A popup keeps a 6 px gap from the
  element it is anchored to unless you set its `margin`; `margin: 0`
  closes the gap.
- **More in the popup.** A popup holds any tree. Put a `col` around
  `Calendar` and add to it, such as `text clock.format("%A")` for the
  day's name.
- **A clock on a bar of its own design.** The clock is a component, so
  `Clock` goes anywhere a node does: in the `start` or `end` of the
  split, or in a panel.

Every element and prop used here is in the
<a href="../reference/elements/index.html">element reference</a>, and
the `clock` and `calendar` services are in the
[service reference](../../docs/reference/services.md).

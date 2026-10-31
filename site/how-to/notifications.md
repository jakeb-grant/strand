# A notification stack

Cards at the top right of the focused monitor, one per notification,
that slide in when a notification arrives and out when it goes. Strand
is the notification server, so nothing else needs to run. The guide's
[level 4](../../docs/guide/level-4.md#notifications) explains it line
by line.

## The file

```strand file=toasts.strand
// toasts.strand
export state dnd = false persist
let shown = notifications.popups.filter(n => !dnd || n.urgency == critical).take(5)

panel Toasts {
  screens: focused; layer: overlay; anchor: top_right; margin: $space.3; keyboard: none
  open: shown.len > 0
  col { width: 380; gap: $space.2
    for n in shown { Toast n }                    // keyed by n.id automatically
  }
}

component Toast(n: Notification) {
  col { pad: $space.3; gap: $space.2; radius: $radius.lg; border: 1, $border
        bg: $surface.alpha(0.88); blur: 24; shadow: $elevation.lg
    enter { x: 420; opacity: 0 }
    exit  { x: 420; opacity: 0; height: 0 }       // siblings slide up to fill the gap
    when hover { bg: $surface.hi }
    when n.urgency == critical { border: 1, $error }
    after n.timeout ?? 6s while !hover && n.urgency != critical { n.expire() }
    on click     { n.activate() }
    on secondary { n.dismiss() }
    row { gap: $space.2
      image n.image ?? n.app.icon { size: 36; radius: $radius.sm }
      col { grow: 1
        text join(" · ", n.app.name, n.summary) { weight: 600; ellipsis: end }
        text n.body { markup: basic; max_lines: 4; color: $fg.muted }
      }
      icon "window-close-symbolic" { size: 14; on click { n.dismiss() } }
    }
    if n.actions.len > 0 {
      row { gap: $space.1
        for a in n.actions { button a.label { grow: 1; on click { a.invoke() } } }
      }
    }
  }
}
```

Only one program can own the notification service on the session bus.
If dunst, mako or another daemon is running, Strand says so, names
it and says how to stop it, and takes the name as soon as it is free. Then send yourself one:

```sh
notify-send "Hello" "from Strand"
```

## What it looks like

Here a first notification arrives, a second comes under it, and the
first is dismissed: it slides out while its height folds, and the card
below slides up into its place. The example gives the panel a fixed
`height`, because every frame of a picture here must be one size; in
your shell the panel grows and shrinks with its cards.

```example toasts
```

## How it works

- **`notifications.popups`** is the list of notifications to show now.
  Each one leaves it when it expires or is dismissed, and its card
  plays its `exit` pose.
- **The `for` is keyed by each notification's id**, which the service
  gives it. When one goes, the other cards keep their state and slide
  to their new places.
- **`after n.timeout ?? 6s while !hover && …`** counts down only while
  its condition holds: pointing at a card pauses it, and a critical
  notification never counts down. The time is the sender's, or six
  seconds when it gave none. `n.expire()` ends the popup as a timeout
  does.
- **`exit { …; height: 0 }`** shrinks the leaving card's height as it
  slides away, so the cards under it move up smoothly.
- **A click** runs the notification's default action (`n.activate()`);
  a right click or the cross dismisses it. A notification whose sender gave
  no default action (`n.can_activate` is false) just closes, or does nothing
  if it is resident.

## Do not disturb

`export state dnd = false persist` is your own switch. `persist` keeps
it across restarts, and `export` lets a key reach it:

```sh
strand toggle toasts.dnd
```

While it is on, the `filter` keeps only critical notifications. The
others still arrive and wait in `notifications.popups`: the server sets
no timers of its own, and only a card counts down. Turn it off and they
show.

## Variations

- **Progress.** `meter n.progress ?? 0` in the card shows how far a
  download or a copy has got, for senders that say (0 to 1).
- **More or fewer at once.** `.take(5)` shows at most five cards;
  change the number.
- **The service's own switch.** `notifications.dnd` is a writable field
  of the service: while it is true, new notifications that are not
  critical skip `popups` altogether (they are still kept in `notifications.all`).

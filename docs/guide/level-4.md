# Level 4: surfaces that come and go

You learn: `on change … after`, `after … while`, `persist`, `export`,
`list`, `Async`; also `let`, `enum`, `match`, `every … while`,
`on show`, `id:`, `@reset`, lambdas (`n => …`), `await` and the `lock`
surface.
You build: an on-screen display for volume and brightness, a
notification stack, an app launcher and a lock screen.

Your bar is always there. Everything on this page appears when
something happens (a key, a notification, a locked session) and goes
away again. Each build is a file of its own beside the five from
level 3; none of them changes the bar.

## An OSD for volume and brightness

Make `osd.strand`:

```strand level=4 file=osd.strand
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

Nothing shows yet. Scroll on the bar's volume row, or drag its slider:
a pill with the level fades in near the bottom of the screen, follows
the volume while you move it, and fades out a little after you stop.

**Keys stay in your compositor.** Strand does not grab keys. Bind your
volume and brightness keys to the command line instead. In sway:

```
bindsym XF86AudioRaiseVolume  exec strand set audio.sink.volume +5%
bindsym XF86AudioLowerVolume  exec strand set audio.sink.volume -5%
bindsym XF86MonBrightnessUp   exec strand set brightness.level +5%
bindsym XF86MonBrightnessDown exec strand set brightness.level -5%
```

In Hyprland the same commands follow `bind = , XF86AudioRaiseVolume,
exec,` and so on. `strand set` writes a service field the way a
handler does. A signed number or percentage is a step from the current
value, and it never goes below 0.

**`osd Level { … }` is a new kind of surface.** A `bar` is on every
monitor all the time. An `osd` is on the focused monitor, above
everything, and clicks go through it to whatever is underneath. It
shows while `open:` is true. Its `enter` pose works as it does on a
node, and the compositor animates it, so the fade costs no repaint.

**`enum Kind { volume, brightness }` declares your own set of
choices.** `state kind = volume` starts as one of them. Wherever Strand
knows a `Kind` is expected, you write the bare name: `kind = brightness`,
`kind == volume`.

**`on change a, b { … }` runs when a value changes.** The first handler
runs when the volume or the mute switch changes, from anywhere: your
keys, the bar's slider, another program. It never runs for the first
value. When you log in, when you save a file and when the default
output switches to your headphones, the value is new to Strand but
nobody changed it, so the OSD stays hidden. That is why it does not pop
up every time the shell starts.

**`on change … after 1.2s` waits for quiet.** It runs once, 1.2 seconds
after the last change. Hold the volume key and the OSD stays up; let go
and it hides 1.2 seconds later.

**`let level = …` is a value you derive.** A `let` is computed from
what it reads and recomputed when any of that changes. You cannot
assign to it: `level = 0` is an error that says so. Use `state` for
what you write and `let` for what follows from it.

`… ? … : …` picks one of two values: when `kind == volume`, the volume
(or 0 while muted); otherwise the brightness. `meter level` draws it as
a filled bar, and `text pct(level)` as a percentage in a fixed-width
font, `4ch` wide, so the digits do not jump.

### The same with `match`

Nested `?` and `:` get hard to read past two choices. `match` names
each case. Replace `osd.strand`:

```strand level=4 file=osd.strand
// osd.strand. Media keys stay in the compositor: strand set audio.sink.volume +5%
enum Kind { volume, brightness }
state kind = volume
state shown = false
on change audio.sink.volume, audio.sink.muted { kind = volume; shown = true }
on change brightness.level                    { kind = brightness; shown = true }
on change audio.sink.volume, audio.sink.muted, brightness.level after 1.2s { shown = false }
let level = match kind {
  volume     => audio.sink.muted ? 0 : audio.sink.volume,
  brightness => brightness.level,
}

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

The OSD behaves exactly as before. **`match kind { … }` picks the arm
whose pattern matches.** Each arm is `pattern => value`. A match on an
enum must cover every variant, so if you add `mic` to `Kind` later,
Strand points at this `match` until you add a `mic =>` arm. `_ =>`
matches anything that is left. In the second arm, `brightness` before
the arrow is the `Kind` variant, since a pattern on `kind` can only be
a `Kind`; `brightness.level` after the arrow is the service.

## Notifications

Strand is a notification server. Make `toasts.strand`:

```strand level=4 file=toasts.strand
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

Send yourself one from a terminal:

```
notify-send "Hello" "from Strand"
```

A card slides in at the top right of the focused monitor and slides
out six seconds later. Point at it and it stays: the six seconds pause
while you hover. Click it for its default action (usually, open what
sent it); right-click it or click the cross to dismiss it. Send several
and they stack; dismiss one and the ones below slide up.

Only one program can be the notification server. If dunst, mako or
another daemon is running, Strand says so instead of starting this one;
stop it first.

**A `panel` is a surface you place.** `screens: focused` puts it on the
focused monitor only, `layer: overlay` above windows, `anchor:
top_right` in that corner. `keyboard: none` means it never takes your
keyboard away from the window you are typing in. It is open while
there is something to show.

**`n => …` is a lambda, a small function written inline.**
`notifications.popups.filter(n => !dnd || n.urgency == critical)` keeps
each popup for which the lambda is true: all of them normally, only the
critical ones while do-not-disturb is on. `.take(5)` keeps the first
five. Both keep each notification's key, so the `for` still knows
which card is which as notifications come and go.

**`after T while cond { … }` is a timer.** It runs once, after `cond`
has held for `T` in total. While the pointer is on the card, `!hover`
is false and the timer pauses; it picks up where it left off when the
pointer leaves. A critical notification never counts down at all. `T`
is the sender's timeout, or six seconds when it gave none.
`n.expire()` ends the popup, as a timeout should.

There is a repeating twin, `every T while cond { … }`, which runs every
`T` while `cond` holds. The lock screen below uses one.

**`exit { … }` is written out here.** Without it, `exit` mirrors
`enter` (level 3). This card enters sliding from the right, and leaves
the same way while also shrinking its height to 0, so the cards below
slide up as it goes rather than jumping.

**`export state dnd = false persist` does two things.**

- `persist` keeps the value across restarts. Turn do-not-disturb on,
  log out, log in: it is still on.
- `export` lets the command line reach it as `toasts.dnd` (the file's
  name, then the value's). Bind a key to `strand toggle toasts.dnd` and
  do-not-disturb is one keypress. Without `export`, a top-level `state`
  or `let` belongs to its file: no other file and no command can read
  it.

**A kept value and a changed default.** Say you later change the
default to `true`. If you never turned do-not-disturb on or off, the
new default simply applies. If you did, Strand keeps your value, and the
reload overlay says it kept it. To throw your value away and start
from the new default, put `@reset` in front of the declaration,
`@reset export state dnd = true persist`, and save: that cell, and
only that cell, starts over. Then take `@reset` out again: while it is
there, every save (of any file) resets the cell again.

### Clear them all, one by one

Replace `toasts.strand`:

```strand level=4 file=toasts.strand
// toasts.strand
export state dnd = false persist
let shown = notifications.popups.filter(n => !dnd || n.urgency == critical).take(5)

panel Toasts {
  screens: focused; layer: overlay; anchor: top_right; margin: $space.3; keyboard: none
  open: shown.len > 0
  col { width: 380; gap: $space.2
    button "Clear all" {
      on click {
        for n in notifications.popups { n.dismiss(); await sleep(80ms) }
      }
    }
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

Send three notifications and click "Clear all": the cards leave one
after another, 80 ms apart, instead of all at once.

**`await` waits inside a handler.** `sleep(80ms)` returns an `Async`
(next section) that finishes after 80 ms, and `await` pauses the
handler until it does. While it waits, the rest of the shell runs on:
other handlers, timers, animations. `for` in a handler runs its block
once for each item of the list as it was when the loop started.

A handler that is waiting is cancelled when its node goes away. That is
why the button is always in the panel rather than inside an
`if shown.len > 1`: with one card left, the `if` would remove the
button, and the loop with it, before the last card went.

## An app launcher

Make `launcher.strand`:

```strand level=4 file=launcher.strand
// launcher.strand. Bind a key to: strand toggle launcher.open
export state open = false
state query = ""
let hits = apps.search(query)          // Async<[Hit]>: fuzzy + frecency, keeps last result

panel Launcher {
  screens: focused; layer: overlay; anchor: center; keyboard: exclusive
  open: <-> open                       // Escape, click-away and focus loss write false
  enter { opacity: 0; scale: 0.96 }    // animated by the compositor, no repaint
  on show { query = "" }
  col { width: 600; pad: $space.3; gap: $space.2; radius: $radius.xl; clip: true
        bg: $surface.alpha(0.82); blur: 32; shadow: $elevation.xl
    input { text: <-> query; placeholder: "Search apps"; font: $font.title; focus: true; nav: results }
    list { id: results; max_height: 420               // virtualised; arrows move selection
      for h in hits {
        row { pad: $space.2; gap: $space.3; radius: $radius.md; align: center
          when hover    { bg: $surface.hi }
          when selected { bg: $accent.container }
          on activate { h.app.launch(); open = false }
          image h.app.icon { size: 32 }
          col { grow: 1
            text h.app.name { marks: h.ranges; mark_color: $accent; weight: 600 }
            text h.app.comment ?? "" { font: $font.caption; color: $fg.muted; ellipsis: end }
          }
        }
      }
    }
    if hits.len == 0 && query != "" && !hits.pending { text "No matches" { color: $fg.muted } }
  }
}
```

Bind a key to open it. In sway:

```
bindsym $mod+d exec strand toggle launcher.open
```

Press it: a search box fades in at the centre of the screen. Type a few
letters and the apps that match appear, the matched letters in the
accent colour, the apps you use most near the top. Arrow keys move the
selection, Return launches it, Escape or a click elsewhere closes the
launcher. Open it again and the box is empty.

**`apps.search(query)` returns an `Async`.** Searching takes a moment,
so the result is not a list but an `Async<[Hit]>`, a value that is
still arriving. It keeps its last result while the next search runs,
so the list never blanks while you type. It also tells you how it is
going:

- `hits.pending` is true while a search is running;
- `hits.error` is the reason the last one failed, or `null`;
- `hits ?? []` is the result, or `[]` while there is none yet.

You cannot forget the waiting case: using an `Async` where a plain value
is expected is an error. A list is read through in the two ways the
launcher uses: `for h in hits` and `hits.len` read the last result,
empty before the first one. `!hits.pending` in the last line stops "No
matches" from flashing while a search is still running.

**`list { … }` is a container for long lists.** It looks like a `col`,
but it only builds the rows you can see, so a thousand apps cost no
more than ten. It also has a selection: arrow keys move it, `selected`
is true on the selected row, and Return fires that row's
`on activate`.

**`id: results` names a node, and `nav: results` points at it.** The
input keeps the keyboard focus so you can type, and `nav:` sends the
arrow keys and Return to the list named `results`. An `id:` also lets
you read another node's booleans, as in `when results.hover { … }`.

**`keyboard: exclusive`** takes the keyboard while the launcher is open,
so you type into it without clicking it first. **`open: <-> open`**
works as the popup's did at level 3: the toggle opens it, and Escape, a
click away or focus moving elsewhere writes `false` back.

**`on show { query = "" }`** runs each time the surface opens. Every
surface has `on show` and `on hide`.

**`await` is the handler-side way to wait for an `Async`.** A binding
reads `.pending` and shows what there is so far; a handler that needs
the finished value waits for it with `await`, as "Clear all" waited
for `sleep`.

## A lock screen

Make `lock.strand`:

```strand level=4 file=lock.strand
// lock.strand. Bind a key, or your idle daemon, to: strand set lock.locked true
export state locked = false

lock {
  open: <-> locked
  state secret = ""
  state dots = 1
  every 400ms while auth.busy { dots = dots % 3 + 1 }
  let checking = match dots { 1 => "Checking.", 2 => "Checking..", _ => "Checking..." }
  bg: $surface
  col { width: 100%; height: 100%; align: center; justify: center; gap: $space.3
    text clock.format("%H:%M") { font: $font.title }
    input { width: 280; type: password; placeholder: "Password"; text: <-> secret; focus: true
      on activate { if secret != "" { auth.submit(secret) }; secret = "" }
    }
    if auth.busy { text checking { color: $fg.muted } }
    else if auth.failed { text "Wrong password" { color: $error } }
  }
}
```

Before you lock anything, check the file: `strand check
~/.config/strand` must report no error in `lock.strand`. Then bind a
key:

```
bindsym $mod+Escape exec strand set lock.locked true
```

An idle daemon can run the same command. Locking before the machine
sleeps needs care: `strand set` returns as soon as `locked` is written,
and the compositor puts the lock up a moment later. If the machine
suspends in between, your unlocked desktop shows for an instant when it
wakes. With swayidle, `-w` makes it wait for the command before the
machine sleeps, and a short pause after `strand set` gives the lock
time to come up:

```
swayidle -w before-sleep 'strand set lock.locked true && sleep 1'
```

Press the key:
every monitor is covered, with the time and a password field. Type your
password and press Return: "Checking..." counts its dots while the
password is checked, and the session unlocks. A wrong password says so
and empties the field.

**`lock { … }` is the lock screen.** It uses the compositor's session
lock (`ext-session-lock`), so while it is up nothing else on your
desktop takes input, and if Strand itself stops, the session stays
locked. A config has at most one `lock`, and it needs no name.

- It is shown while `open:` is true. `strand set lock.locked true`
  locks, and a successful unlock writes `false` back through `<->`.
- **`auth.submit(secret)` checks the password** in a small separate
  helper, through PAM: the `strand` PAM service, or `login` with a
  one-time warning when `/etc/pam.d/strand` is missing. Only a success
  unlocks. A `lock` in which nothing calls `auth.submit` is an error:
  it would have no way out.
- `auth.busy` is true while a password is being checked, and
  `auth.failed` after one was refused. The field sends nothing when it
  is empty, because an empty Return pressed to wake the screen would
  otherwise count as a failed attempt.
- `auth.message` is what PAM last told you, as it says it: "Place your
  finger on the fingerprint reader" when your PAM stack has a
  fingerprint reader, which then unlocks with no password at all. A
  lock that might meet one shows it:
  `if auth.message != "" { text auth.message }`.
- Which monitors, which layer, where and how it takes the keyboard are
  the session lock's to decide: every monitor, above everything,
  filling it, always. Setting `screens`, `layer`, `anchor` or
  `keyboard` on a `lock` is a warning that says so.
- **It fails closed.** If anything in your lock faults, a built-in
  password field appears in its place, so you can always unlock. Edits
  to the lock that you save while locked wait until you unlock.

**`every 400ms while auth.busy { … }`** is the repeating timer from the
toasts section. While a check runs it steps `dots` through 1, 2, 3,
1, … (`%` is the remainder); when the check ends it stops, and nothing
runs until the next one.

**`match dots { 1 => …, 2 => …, _ => … }`** matches numbers too. A
match on a number has no fixed list of cases, so it must end with `_`.

**`let checking`** sits inside the `lock`, next to its `state`. A `let`
can go in a surface's or component's tree as well as at the top of a
file, next to the values it reads.

## What you have

Your level-3 bar, plus four surfaces that appear when they are needed.

| File | Holds | Opened by |
| --- | --- | --- |
| `osd.strand` | the `Level` OSD | a volume or brightness change |
| `toasts.strand` | the `Toasts` panel and `Toast` | a notification |
| `launcher.strand` | the `Launcher` panel | `strand toggle launcher.open` |
| `lock.strand` | the lock screen | `strand set lock.locked true` |

Next, [level 5](level-5.md) gives the shell a theme of your own, then
builds services of your own, a control centre and a dock.

# Motion

Every visual prop in Strand moves to its new value on a spring. You do
not ask for an animation: when a value changes, whether from state, a
service, a theme swap or a saved file, the renderer carries the prop
from where it is drawn to where it should be.

## Springs by default

Movement and size spring along `$motion.spatial`; colour and opacity
along `$motion.effects`. The built-in theme sets them, with a third,
looser spring for overrides:

```strand fragment
motion { spatial: spring(3000, 0.75); effects: spring(1600, 1); bouncy: spring(380, 0.75) }
```

A spring is a stiffness and a damping ratio. Below a ratio of 1 it
overshoots a little and settles; at 1 it arrives without overshooting.
Your own theme can change any of them, and every prop that uses the
token follows.

## Choosing a curve with `~`

A prop takes its own curve after `~`: a spring (`$motion.bouncy`,
`spring(500, 0.5)`), a duration (`300ms`), a named curve or a cubic
bézier, or `instant`.

```strand fragment
x: on ? 196 : 0 ~ $motion.bouncy
```

The three knobs below move when `on` flips. The first uses the default,
the second `$motion.bouncy`, the third a timed `300ms`. Scrub through
the frames to see each one, and read the curves under the picture: the
legend gives each curve as the renderer resolved it.

```example springs
```

## Interrupted motion keeps its speed

A value that changes while its prop is still moving retargets the
spring from where it is, with the velocity it has, so an interrupted
animation never jumps or restarts.

## A curve per direction

The `~` expression is read with the value, so one prop can come and go
at different speeds:

```strand fragment
opacity: open ? 1 : 0 ~ (open ? 160ms : 1s)
```

fades in over 160 ms and out over a second. A `~` written in a `when`
applies while that `when` wins, so the same reads:

```strand fragment
opacity: 0 ~ 1s
when open { opacity: 1 ~ 160ms }
```

Changing only the curve moves nothing.

## Coming and going: `enter` and `exit`

`enter { … }` is where a node animates in from, and `exit { … }` where
it animates to before it goes. Without an `exit`, a node leaves the way
it came.

```strand fragment
enter { opacity: 0; scale: 0.96 }
exit  { x: 420; opacity: 0; height: 0 }
```

Poses work on surfaces, the branches of an `if`, the copies of a `for`,
list rows and pages. When a node is added, removed or moved, its
siblings glide to their new places: a leaving card with `height: 0` in
its `exit` lets the cards under it slide up.
[A notification stack](../how-to/notifications.md) shows it frame by
frame, and [the OSD](../how-to/osd.md) a surface springing in.

A panel, OSD or popup that is the size of what it holds springs its
own size when that changes, growing and shrinking alike.

## Timed animations: `keyframes`

For a movement that is not a change of value, such as a shake or a
pulse, declare keyframes and play them from a handler:

```strand file=ask.strand
keyframes shake { 0%, 100% { x: 0 }; 25% { x: -6 }; 75% { x: 6 }; duration: 300ms }
state answer = ""

panel Ask {
  anchor: center; keyboard: exclusive
  input { width: 240; text: <-> answer; placeholder: "Type yes"; focus: true
    on activate { if answer != "yes" { play shake } }
  }
}
```

`play shake` plays it once on the node the handler is on: here the
input shakes when Return is pressed on anything but "yes".

## What cannot spring

Some values have no in-between, and change at once: fonts, and layout
lengths such as `pad` (a paint offset such as `x` springs instead). A
list of shadows is padded to the same length, then springs.

With the desktop's reduced-motion setting on, every transition is
instant, and looping animations and effects stop.

## What moves without relayout

`x`, `y`, `scale`, `rotate` and `opacity` are paint-only: animating
them never lays the tree out again. A size that springs lays out only
the part of the tree under the nearest ancestor whose size stays put,
and reordered list items glide to their new places.

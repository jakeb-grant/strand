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

## What moves without relayout

`x`, `y`, `scale`, `rotate` and `opacity` are paint-only: animating
them never lays the tree out again. A size that springs lays out only
the part of the tree under the nearest ancestor whose size stays put,
and reordered list items glide to their new places.

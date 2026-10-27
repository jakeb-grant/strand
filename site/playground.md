# Playground

Every example on this site, running in your browser. It is Strand's
own compiler and renderer, with the mock services the docs use and the
steps each example's scenario plays. Pick an example, play its steps or run them one at a time, point
at it and click, and edit its config: it runs again as you type, and a mistake
shows what `strand check` would print.

```playground hello springs osd toasts swap
```

## What runs here

The examples draw with the CPU renderer, the same code that draws a bar
on your screen, into a canvas. Time is a virtual clock that ticks every
1/60 s of real time, so motion runs at the speed it would on your
desktop. Bars, panels, OSDs and popups are shown; windows and lock
screens are not, nor is the GPU's work. Service values are the mock's,
the icons are the few files the examples draw, and the fonts are
Liberation Sans, regular and bold. Nothing is downloaded until you open
this page.

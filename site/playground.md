# Playground

The two examples below run in your browser. The page loads Strand's
compiler and renderer, built to WebAssembly, and runs each config the
way `strand run` would, with the mock services the docs use. Change
the config and press Run (or Ctrl+Enter); an error shows what
`strand check` would print.

<link rel="stylesheet" href="assets/playground.css">
<script src="assets/playground.js" defer></script>

## The hello bar

The bar from the front page, on a 640 px screen. The clock reads the
mock's fixed time; the title and battery come from the service values
under the config.

<div class="pg" data-example="hello"><p>Loading the live example…</p></div>

## Springs

Three knobs move when `on` flips: on the default spring, on
`$motion.bouncy`, and on a timed `300ms`. Press `on: false` to flip it.

<div class="pg" data-example="springs"><p>Loading the live example…</p></div>

## What runs here

The examples draw with the CPU renderer, the same code that draws a
bar on your screen, into a canvas. Bars, panels and popups are shown;
windows, icons and images from files, and the GPU's effects are not.
Services are the mock's values, which you can edit. Hover delays use
the page's clock; everything else runs on a clock that advances one
tick per 1/60 s.

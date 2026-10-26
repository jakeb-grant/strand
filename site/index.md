<section class="hero">
<div class="hero-copy">

# Strand

<p class="hero-lede">A toolkit for Wayland shells.</p>

<p class="hero-text">Write bars, launchers, lock screens and panels in one small typed language. Colours, sizes and positions spring, saving a file never restarts the shell, and nothing runs while nothing changes.</p>

<p class="hero-actions"><a class="button" href="guide/level-1.html">Build the hello bar</a><a class="text-link" href="guide/index.html">Read the guide</a></p>

</div>

```example hello
```

</section>

<section class="band band-reload">
<div class="band-copy">

## Saving changes the shell, not its state

Strand watches your config. When you save, it reconciles the running
shell with the new file: what you changed animates to its new value,
and the rest keeps its state, an open popup or the text typed into a
launcher. A save that does not check leaves the last good shell
running, and an overlay points at the line.

</div>
<dl class="figures">
<div><dt>35 ms</dt><dd>from saving a token edit to the frame on screen, at the 95th percentile. CI fails a build that is slower.</dd></div>
<div><dt>50 ms</dt><dd>for an edit that adds or removes an element, held the same way.</dd></div>
</dl>
</section>

<section class="band band-motion">
<div class="band-copy">

## Changes spring into place

Change a value and its prop moves there. Spring curves are theme tokens
you can retune, and a prop can choose its own after `~`. These frames
are Strand's own, drawn one at a time on a virtual clock. Try it live
runs the same config in your browser, Strand's compiler and renderer
built to WebAssembly, and draws the same frames, pixel for pixel; edit
the config and it runs again.

</div>

```example springs
```

</section>

<section class="band band-check">
<div class="band-copy">

## Checked before it runs

Every name, field and type is known before the shell starts. `strand
check` and your editor's language server say what is wrong and where,
and usually how to fix it. This error is computed by the same check
when this site is built.

</div>

```strand-check
bar Top {
  edge: top; height: 32
  text pct(battery.precent)
}
```

</section>

<section class="band band-idle">
<div class="band-copy">

## Idle when nothing changes

With nothing else changing, the hello bar wakes once a minute, to
turn its clock. A surface repaints only what changed, and the GPU
draws large animations and shaders. CI holds design.md's bar on two
monitors to 48 MiB, measured with no GPU device.

</div>
<ul class="compositors" aria-label="Compositors tested in CI">
<li>sway</li><li>Hyprland</li><li>niri</li><li>labwc</li>
</ul>
<p class="compositors-note">Every build runs on these four compositors in CI.</p>
</section>

<section class="closing">

## Start with one file

<p>Level 1 of the guide builds the hello bar from <code>strand new</code> in a few minutes. Each level after it adds a handful of ideas, up to a full desktop shell.</p>

<p class="hero-actions"><a class="button" href="guide/level-1.html">Build the hello bar</a><a class="text-link" href="reference/elements/index.html">Browse the reference</a></p>

</section>

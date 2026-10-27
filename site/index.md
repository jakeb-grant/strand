<section class="intro">

# Strand

<p class="lede">A toolkit for Wayland shells. Write bars, launchers, lock screens and panels in one small typed language; save the file and the running shell follows.</p>

<p class="actions"><a class="btn primary" href="start/install.html">Install</a><a class="btn" href="guide/index.html">Read the guide</a></p>

</section>

<section class="hello">

```example hello
```

</section>

<section class="row">
<div class="row-copy">

## Changes move on springs

Change a value and its prop moves there. Spring curves are theme tokens
you can retune, and a prop can choose its own after `~`.

<p><a href="concepts/motion.html">Motion</a></p>

</div>

```example springs
```

</section>

<section class="row">
<div class="row-copy">

## Save, and the shell follows

Strand watches your config. When you save, what you changed animates to
its new value, and the rest keeps its state, an open popup or the text
typed into a launcher.

Every name, field and type is known before the shell starts. A save that
does not check leaves the last good shell running, and the check says
what is wrong and where, and usually how to fix it.

<p><a href="concepts/live-reload.html">Live reload</a></p>

</div>

```strand-check
bar Top {
  edge: top; height: 32
  text pct(battery.precent)
}
```

</section>

<section class="docs">

## The documentation

<nav class="index" aria-label="The documentation">
<a href="start/install.html"><b>Start</b><span>Install Strand and run a shell.</span></a>
<a href="guide/index.html"><b>Guide</b><span>Five levels, from one bar to a full shell.</span></a>
<a href="how-to/calendar-popup.html"><b>How-to</b><span>Complete configs for common pieces.</span></a>
<a href="concepts/language.html"><b>Concepts</b><span>The language, state, theming, layout and motion.</span></a>
<a href="tools/editor.html"><b>Tools</b><span>Your editor, the strand command and the inspector.</span></a>
<a href="reference/elements/index.html"><b>Reference</b><span>Every element, service, type and function.</span></a>
</nav>

<p class="note">Strand runs on sway, Hyprland, niri and labwc, and every build is tested on all four.</p>

</section>

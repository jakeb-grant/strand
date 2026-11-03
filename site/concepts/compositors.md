# Strand and your compositor

Strand draws every bit of its own motion: springs, `enter` and `exit`,
a `morph` from one surface into another. The compositor's part is to
place and stack the surfaces Strand gives it. If the compositor also
animates them, the two animations play over each other. A launcher
meant to grow out of a bar fades in through it instead, and a panel can
jump as it comes to rest.

This page says what Strand does so the compositor has nothing to
animate, and what each compositor needs from you.

## Who animates what

| | Strand | The compositor |
| --- | --- | --- |
| A surface opening and closing | its `enter` and `exit`, drawn in its own buffer | shows it, and takes it away |
| A box growing, shrinking, moving | springs inside the surface | nothing |
| A box becoming another (`morph`) | from the one box to the other, across surfaces | nothing |
| Where the surface sits | asks once, as it opens | places it, keeps it there |

A compositor that animates layer surfaces breaks the last two columns.
Hyprland, by default, fades a layer surface in when it appears and out
when it goes, and slides it when it changes size or place. Its fade
starts from nothing just as Strand draws the panel's first frame: in
the middle of a morph you see both surfaces at once, one of them half
transparent.

## What Strand does about it

Once a bar, panel or OSD is on screen, Strand never asks the
compositor to resize it, move it or change its anchors, so there is
nothing to slide:

- **A panel anchored to one edge** (`anchor: top` with no `width`) is
  stretched along that edge by the compositor itself, from its first
  frame. What it holds is laid out and springs inside it.
- **Shadows get their room once.** A panel keeps room on each side for
  the farthest any of its shadows, glows or `attach` fillets can reach,
  hover and `when` shadows included, from its first frame until the
  config is saved again.
- **On Hyprland, a panel's own moves are painted.** An `enter { y: -14 }`
  or a `slide` moves the box inside the surface, rather than the
  surface's margins. Its fade still goes to the compositor, which only
  changes its opacity.

What Strand cannot stop is the animation a compositor plays as a
surface appears and disappears. No Wayland protocol lets a program ask
for none: on Hyprland, only a rule in your own config turns it off.

## What your compositor needs

### Hyprland

Run `strand compositor-rules` and paste what it prints into your
Hyprland config (`~/.config/hypr/hyprland.lua`). It always prints this
rule, which turns Hyprland's layer animations off for every surface
Strand makes:

```lua
hl.layer_rule({
    name = "strand-no-anim",
    match = { namespace = "^strand-.*$" },
    no_anim = true,
})
```

Then the blur rules for the surfaces that ask for `blur`, if your
Hyprland has no `ext-background-effect-v1`. Hyprland reads a saved
config at once; `hyprctl reload` makes sure, and the rule applies to
each surface as it next appears.

Hyprland matches a rule's `namespace` against the whole namespace, not
a part of it: `"^strand-"` alone matches no surface, and the rule does
nothing. Keep the `.*`.

With the rule, a surface with no `exit` simply goes when it closes,
with no fade. Give it one (`exit { opacity: 0 }`) if you want one.

`--classic` prints the same rules as `layerrule` lines for a hyprlang
config, which Hyprland reads no more from 0.57.

### niri

Nothing. niri does not animate layer surfaces as they appear or go.
Its `layer-rule`s match a namespace anywhere in it (`match
namespace="^strand-"` works), for its own shadows and background
effects.

### sway and labwc

Nothing. Neither animates layer surfaces.

### Another compositor

If yours animates layer surfaces, turn that off for Strand's: every
namespace Strand uses starts with `strand-`.

## Strand's surface names

Each layer surface has a namespace a rule can match:

| Surface | Namespace |
| --- | --- |
| `bar Top`, `panel Launcher`, `osd Level` | `strand-Top`, `strand-Launcher`, `strand-Level` |
| a panel's `scrim` | `strand-<Name>-scrim` |
| the surface that catches a click away from an open panel | `strand-<Name>-click-away` |

`hyprctl layers` and `niri msg layers` list the namespaces on screen.

## Why a rule, not a workaround

Other shells meet the same animations and solve them one of two ways.
Some (end-4's, Noctalia, DankMaterialShell) ship the same rule. Others
(caelestia) never let a surface appear or go while the shell runs: each
stays mapped, empty and without input, while closed, so there is
nothing for the compositor to animate.

Strand could do the second, but every closed panel would then be a
surface on every monitor, the keyboard would have to be passed around
by hand, and a surface kept on the `overlay` layer stops Hyprland from
sending a fullscreen game straight to the screen. A rule in your
compositor's config costs nothing and changes nothing else, so that is
what Strand asks for.

<!-- repo-only -->
How this is held: `a_mapped_layer_surface_sends_no_layer_state_after_its_first_configure`
(`crates/strand-surface/tests/fake.rs`) drives the renderer through the
surface manager and fails on any size, margin, anchor or zone asked for
after a surface's first configure. Hyprland itself is run nested
(`scripts/container/hyprland-nested.sh`); the rule's full match is
Hyprland 0.57's `RE2::FullMatch`
(`src/desktop/rule/matchEngine/RegexMatchEngine.cpp`), and niri's
layer rules have no open or close animation (`src/layer/mapped.rs`,
`docs/wiki/Configuration:-Layer-Rules.md`, at a8562a1). decisions.md
compositor-motion has the research.
<!-- /repo-only -->

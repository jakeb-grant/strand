# The Strand guide

This guide takes you from `strand new` to a full desktop shell in five
levels. Each level teaches a handful of ideas and ends with something
you can use. Levels 1 to 3 build one bar, on every monitor, with
workspace dots, the focused window's title, a clock with a calendar
popup, a volume slider, the battery and the tray. Level 4 adds the
surfaces that come and go (an on-screen display, notifications, a
launcher, a lock screen and an admin password dialog), and level 5
your own theme, services, a control centre and a dock.

Nothing at level N+1 appears in a level-N example. If you stop after
any level, everything you have written still works.

| Level | You learn | You build | Page |
| --- | --- | --- | --- |
| 1 | Surfaces, nodes, props, services as live values | The hello bar | [level-1.md](level-1.md) |
| 2 | `$tokens`, `component`, `for`, `when`, `on click` | Workspace dots, tray | [level-2.md](level-2.md) |
| 3 | `state`, `<->`, `if`, `popup`, `enter` | Clock with calendar, volume slider | [level-3.md](level-3.md) |
| 4 | `on change … after`, `after … while`, `persist`, `export`, `list`, `Async` | OSD, notifications, launcher, lock screen, password dialog | [level-4.md](level-4.md) |
| 5 | `tokens`, spring overrides `~`, `service from dbus`, `pages`, drag and drop, shaders, `file` | Control centre, dock, custom services | [level-5.md](level-5.md) |

## Every construct and its level

The table above is design.md's learning ladder. The constructs its "You
learn" column does not name are taught at these levels, as the ladder's
"Also taught there" column says (decisions.md, m5-docs, one paragraph
each).

| Level | Also taught here |
| --- | --- |
| 1 | `split` (with `start`, `center`, `end`); comma shorthands; expressions (`?.`, `??`, operators, calls such as `pct` and `join`) |
| 2 | `on secondary`; `hit:`; the node booleans `hover` and `pressed` |
| 3 | assignment and compound assignment (`=`, `+=`, `-=`, `*=`, `/=`); `else`; `exit`; `on scroll(dy)` |
| 4 | `let`; `enum`; `match`; `every … while`; `on show`; `id:`; `@reset`; lambdas (`n => …`); `await`; the `lock` surface |
| 5 | `set { }`; `keyframes` and `play`; `canvas`; `type` and `key`; `slot`; `fn`; a node's `box` and `pointer`; settings files (`state x from "file.toml" { … }`); SVG layer selectors (`#name { … }`); `service … from file`, `from dir`, `from listen`, `from poll` and `permit exec`; `action` |

## How to read the code blocks

Every code block marked `strand` is the **whole** file it names, as it
should be after that step. Copy it over the file and save; you never
merge a fragment by hand. The block's tag says its level and its file:

````
```strand level=2 file=dot.strand
component Dot(ws: Workspace) { … }
```
````

Steps run in page order, level 1 first. A file a step does not show
stays as it was. `crates/strand-dev/tests/guide.rs` applies every
step in that order to one config directory and checks it as
`strand check` does after each one (`every_guide_block_checks`), so a
block that does not check, or that `strand fmt` would change, fails the
build. It also holds the copies of design.md's blocks to the accepted
fixtures, and runs the dock and the control centre on mock services.
`crates/strand-dev/tests/ladder.rs` holds every step to the ladder: no
block uses a construct placed above its level, and each construct is
first written (or, for the few taught in prose, named) at its own
level. The newcomer test (`crates/strand/tests/m5/newcomer.rs`) goes
further for levels 1 to 3: it runs `strand new`, saves each block into
a running shell on two monitors, and checks after every step that
`strand check` is clean, which state cells the save kept, and each
monitor's bar against a reference picture.

A step can also need a file that is not `.strand`, such as level 5's
template. Its block is tagged with its own language and the same
`level=` and `file=`, and is that file's whole content too:

````
```css level=5 file=colors.css.tmpl
:root { … }
```
````

Short pieces of syntax inside a sentence (`when hover { … }`), and
untagged blocks (commands to type, keybind lines for your compositor),
are not steps.

## What you need

- A Wayland compositor with layer-shell: sway, Hyprland, niri, labwc,
  river or similar. GNOME does not offer layer-shell.
- The `strand` binary on your `PATH`. There is no release yet, so
  either build it from a checkout of the repository with
  `cargo build --release -p strand` (it lands in
  `target/release/strand`), or take the `strand-release-binaries`
  artifact of a green run of its CI workflow on GitHub and rename
  `strand-gpu` in it to `strand` (`strand-cpu` is the same without the
  GPU backend). `strand --version` tells you it is found.

Then start at [level 1](level-1.md).

## More pages

- [Plugins](plugins.md): a service written in Rust and built into your
  own `strand` binary (level 5, section 7).
- The language itself, in full: [`docs/spec.md`](../spec.md) for what
  each construct means, [`docs/grammar.md`](../grammar.md) for how it
  is written.
- Every element, prop, service, type, function and token, with its
  doc: [`docs/reference/`](../reference/README.md), generated from the
  schemas by `strand-dev reference`.

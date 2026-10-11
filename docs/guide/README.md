# The Strand guide

This guide takes you from `strand new` to a full desktop shell in five
levels. Each level teaches a handful of ideas and ends with something
you can use. Levels 1 to 3 build one bar, on every monitor, with
workspace dots, the focused window's title, a clock with a calendar
popup, a volume slider, the battery and the tray.

Nothing at level N+1 appears in a level-N example. If you stop after
any level, everything you have written still works.

| Level | You learn | You build | Page |
| --- | --- | --- | --- |
| 1 | Surfaces, nodes, props, services as live values | The hello bar | [level-1.md](level-1.md) |
| 2 | `$tokens`, `component`, `for`, `when`, `on click` | Workspace dots, tray | [level-2.md](level-2.md) |
| 3 | `state`, `<->`, `if`, `popup`, `enter` | Clock with calendar, volume slider | [level-3.md](level-3.md) |
| 4 | `on change … after`, `after … while`, `persist`, `export`, `list`, `Async` | OSD, notifications, launcher, lock screen | [level-4.md](level-4.md) |
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
| 5 | `set { }`; `keyframes` and `play`; `canvas`; `type` and `key`; `slot`; `fn`; settings files (`state x from "file.toml" { … }`); SVG layer selectors (`#name { … }`); `service … from file`, `from listen`, `from poll` and `permit exec` |

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
stays as it was. The guide's test, still to come (M5 wave 3,
`crates/strand/tests/m5/newcomer.rs`), will apply the blocks in the
same order to a running shell on two monitors and check each step's
frame, so that a block that is wrong fails the build. Until it lands,
nothing checks the blocks automatically.

Short pieces of syntax inside a sentence (`when hover { … }`) are not
steps.

## What you need

- A Wayland compositor with layer-shell: sway, Hyprland, niri, labwc,
  river or similar. GNOME does not offer layer-shell.
- The `strand` binary on your `PATH`.

Then start at [level 1](level-1.md).

## Pages still to come

- [level-4.md](level-4.md) and [level-5.md](level-5.md) are outlines
  (M5 wave 2).
- The language itself, in full: [`docs/spec.md`](../spec.md) for what
  each construct means, [`docs/grammar.md`](../grammar.md) for how it
  is written.
- Every element, prop and service field: `docs/reference/`, generated
  from the schemas (M5 wave 2).

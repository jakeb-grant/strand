# Inspector

The inspector shows what Strand knows about one element of your running
shell: where it comes from in your files, where it was laid out, which
tokens its colours come from and which of its state was kept. The shell
draws only the outline of the picked element and the repaint flashes;
the rest runs in a terminal, in `strand-dev inspect`.

## Start it

Build `strand-dev` as [Editor setup](editor.md#the-language-server)
says, start your shell with `strand run`, then in a terminal:

```sh
strand-dev inspect
```

It connects to the shell's socket, by the same rule the `strand`
commands use (`$STRAND_SOCKET`, else the socket in `$XDG_RUNTIME_DIR`
named for your display), or to the socket path you give it. It starts
in pick mode: click an element on any monitor. While you pick, clicks
go to the inspector, not to the shell's handlers. Enter stops picking
without a pick.

One click picks one element, and the shell takes its input back. The
element stays outlined, and the terminal shows its pane. A lock screen
is never picked, and pick mode ends when the screen locks.

## The pane

The inspector's own test draws this pane for a `box`:

```text
box 4.0  /c/bar.strand:3:5
  identity  [DP-2]  id: target  #0  in bar
  layout    surface 2: 0,0 200×40
  props
    bg: #f38ba8
    width: 200
  tokens
    bg ← $probe.c (base) = #f38ba8
  state
    bar.clicks [kept]
```

- The first line is the element's kind, its node id and where it is
  written.
- **why**, for a `shader` node that draws nothing: the reason, such as
  no GPU.
- **identity**: what the shell knows the element by across reloads: its
  place in the instance (here the bar on `DP-2`), its `id:`, its
  position among siblings of the same kind, and the scope it is in.
- **layout**: the box it was drawn in, on each surface that shows it,
  or "not drawn".
- **props**: its props as the shell holds them now.
- **tokens**: for each prop that reads a token, the chain of tokens back
  to where the value is set, with where each is defined and its value.
- **state**: the state cells of the component, surface and list item
  the element is mounted in, then the cells its props read, each with
  its badges: `[kept]` while a cell holds a value a reload kept,
  `[persisted]` for a `persist` cell, and the layer a setting's value
  comes from.

## Commands

Each command is a line typed into the inspector's terminal.

| Command | What it does |
| --- | --- |
| `p` | pick an element |
| `r` | reset the element's subtree: its state back to the defaults |
| `e` | open the element's source in `$VISUAL` or `$EDITOR` at its line |
| `set <prop> <value>` | write a literal prop value back to the element's source |
| `token <path> <value>` | write a token's literal value back to its definition |
| `f` | turn repaint flashing on or off |
| `?` | list the commands |
| `q` | quit (Ctrl-C too) |

When the inspector quits, the shell ends pick mode, the outline and the
flashing.

**Repaint flashing** tints each area the shell repaints for 300 ms, so
you can see what a change redraws.

## Writing back

`set` and `token` change your file, and the shell then reloads it as it
would after a save:

```text
set width 240
token accent #7aa2f7
```

They write only a literal: a number, a colour, a string. A value that is
an expression, a binding or a reference to a token is refused, since
changing it would change what the value means. A token that the palette
or an overlay sets is refused too.

The file must still be the one the shell compiled. If it changed since,
the edit is refused: save it so the shell reloads, then edit again. The
new text replaces the file atomically and follows symlinks, so a linked
config stays linked. A file you cannot write, such as one in the Nix
store, is refused with the reason, and the inspector offers to open it
in your editor instead.

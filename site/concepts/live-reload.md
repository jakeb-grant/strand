# Live reload

Saving a file is a change of state, not a restart. While `strand run`
is running, every save is checked, and what changed is merged into the
running shell: an open launcher keeps its query, a calendar its month,
and a colour you changed glides to its new value.

## What each edit does

| You edit | What happens | State kept? |
| --- | --- | --- |
| A token's value | The colours spring to it | Yes |
| A prop or binding | Patched in place; animates from where it is | Yes |
| A node added or removed | Its `enter` or `exit` plays; its siblings glide | Yes, elsewhere |
| A `state`'s default | Taken only if the value was never changed | Yes, if you changed it |
| A `state`'s name or type | That one cell resets, with a warning | No, that cell only |
| A handler's code | Restarted; an `await` in progress is cancelled and reported | Yes |
| A timer's duration | The time left is rescaled | Yes |
| A surface's layer, namespace or kind | Only that surface is made again | Yes |
| A custom service's declaration | Only that service restarts; built-ins never do | Yes |
| Anything in the `lock` | Waits until you unlock | Yes |

A handler counts as changed when its own code changed, or a `fn` or
`let` it reaches did. An unchanged handler keeps what it was doing.

## How Strand keeps state

Each node keeps its identity while its source text still maps to it.
Editing a node's own props never resets it, and wrapping it in a
container keeps it. After that a `key` or `id` decides, then the
node's place among siblings of the same kind. A node whose kind
changed is a new node. Where the edit is truly ambiguous, the state
resets with a warning: Strand never guesses.

Because state lives on components, surfaces and list items, never on a
leaf element, editing an element cannot take a popup's open state with
it. Nodes moved into another component are new nodes there, and their
state starts over.

A surface's state is keyed by its place in the program, so renaming a
surface keeps it. A bar's state on each monitor is keyed by the
monitor and survives a reload.

## Changed defaults

A cell takes a new default only if it still holds the old one. If you
changed it, it keeps your value, and the reload overlay says so:

```text
launcher.query: kept "fir" (default changed) [reset]
```

A value a reload writes never fires `on change`, so an OSD does not
flash on save.

## When a save has errors

A save is checked before anything changes:

1. Strand waits 15 ms after the last write, so "save all" in your
   editor is one reload.
2. A file whose content did not change stops here.
3. The changed files are checked with everything that refers to them,
   off the drawing path; the shell keeps running meanwhile.
4. The largest set of changes that checks clean is applied in one
   step, with no blank frame. The rest waits with its diagnostics.
5. On an error the running shell stays as it was. The last good
   sources are kept, so a config that is broken when Strand starts runs
   its last good version.

If an error is still there 250 ms after the save, an overlay opens
listing every diagnostic, with where it is and a fix where there is
one. Clicking one opens the file at that line in your editor
(`$STRAND_EDITOR`, `$VISUAL` or `$EDITOR`). A save you fix within
the 250 ms, such as one your editor formats, never opens it.

A fault while the shell runs, such as a value that cannot be computed,
freezes only its own component, outlined in red, until a save fixes
it. The rest of the shell runs on.

## Starting over

- **`@reset`** on a `state` resets that one cell to its default when
  the save is applied, and again on every later reload while it is
  written, so remove it after one save.
- **`strand reload --hard`** drops every state that is not `persist`
  and makes every surface again. Settings files are untouched.
- **`strand reload`** rescans the config now.

## Watching reloads

`strand watch` prints each reload as it happens: the files, what kind
of edit it was, which cells were kept or reset, how long it took and
its diagnostics. `strand watch --json` gives the same as JSON lines.

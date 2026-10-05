# Decisions

Interpretations of ambiguities in `design.md`, one short entry each.

## compiler

- **2026-10-05 · Keywords are contextual.** No reserved words; a word is a
  keyword only in its position, and `ident:` at the start of an item is
  always a prop (`enter: popin(0.8)`, `type: password`, `text: <-> q`).
  Removes a concept: users never learn a reserved-word list.
- **2026-10-05 · Calls touch.** `f(x)` and `xs[0]` need the `(`/`[` to touch
  the callee, as CSS function tokens do. That is what lets space-separated
  values hold parenthesised terms (`0 (-2px) 8px $shadow`), and an
  accidental `f (x)` gets a "remove the space" fix.
- **2026-10-05 · Operators always bind in space-separated values.** `0 -2px`
  is subtraction, never a negative term, so whitespace never changes meaning
  (design.md "Bad" table). Negative terms in shadows need parentheses.
- **2026-10-05 · Value precedence.** In a prop value: `~` loosest, then `,`,
  then space separation, then expression operators. So
  `lg: 0 8px 24px $a, 0 1px 2px $b` is a two-shadow list and
  `margin: 8, 8, 0 ~ instant` springs the whole value.
- **2026-10-05 · Line structure.** Newlines are ignored in `()`/`[]`,
  significant in `{}`. A line continues only if it starts with a binary
  operator, `.`, `?.`, `?`, `:`, `~`, `=>`, `<->` or `else`, or the previous
  line ended with an operator or comma. Optional blocks (element bodies,
  prop sub-blocks) must open on the head's line; mandatory ones may open
  on the next.
  Clause words (`key`, `persist`, `while`, `after`, `extends`, `rw`) stay on
  their line.
- **2026-10-05 · `pages current: page { … }`.** An element's single head
  argument may be named; `kind name: e { … }` is sugar for a first prop.
- **2026-10-05 · No record literal.** Records are built with call syntax and
  named arguments, `Pin(app: a, label: "x")`, reusing named args.
- **2026-10-05 · Component tokens in the head.** As written in design.md:
  `component Toast(n) tokens { radius: $radius.lg } { body }`. Parameter
  types are optional in the grammar (design.md writes `Toast(n)`); the
  checker requires them where it cannot infer.
- **2026-10-05 · `fn` has one form.** `fn f(x: T) -> U { …; value }`; the
  value is the last statement. No `= expr` short form (one syntax per idea).
- **2026-10-05 · `await` and `play` parse anywhere expressions or items
  do;** the checker limits `await` to handlers and decides what a tree-level
  `play` means.
- **2026-10-05 · Service sources.** `dbus` takes `system|session`, the bus
  name and an optional object path (UPower's display device lives off the
  derived path); `file` and `listen` take one expression; `poll` takes a
  command and `every T`. `permit exec ["prog", …]` is valid at the top level
  and inside a service.
- **2026-10-05 · `#word`.** A colour in expression position, an SVG
  selector at the start of an item (`svg "x.svg" { #needle { … } }`).
- **2026-10-05 · Token keys may carry `$`.** `$surface.hi: …` (design.md's
  token table) and `surface.hi: …` (its theme file) name the same token.
- **2026-10-05 · Small lexical choices.** `//` comments only; strings are
  single-line with escapes and no interpolation; `%` touching digits is a
  unit, after a space it is remainder; a number right after `.` takes no
  fraction (`$space.2`); comparisons do not chain; `??` binds looser than
  `||`.
- **2026-10-05 · Surface names.** `bar`, `panel` and `osd` need a name (it
  becomes the `strand-<Name>` namespace); `lock` may omit it.
- **2026-10-05 · Nesting limit 128.** Blocks, brackets, and operator/call
  chains all count. Measured: 256 fits a 2 MiB stack in a debug build, 512
  does not; 128 leaves room for later passes over the same tree.
- **2026-10-05 · Missing `}` recovery.** A top-level declaration keyword at
  column 0 inside an open block closes the open blocks with one "unclosed
  `{`" error, and a `}` indented unlike its `{` marks that block as the
  likely culprit, so the error points at the right line (design.md "What
  you see" #3).
- **2026-10-05 · `strand check`.** Loads `.strand` files up to three
  directories down (the watcher's depth), follows symlinks, skips hidden
  directories, and treats an empty or missing directory as an error. Until
  the checker lands it reports syntax diagnostics only.
- **2026-10-05 · Diagnostics live at `strand_compiler::diagnostic`,** not
  under `syntax`, because the checker and reconciler will share them.
- **2026-10-05 · Snippet fixtures.** design.md's code blocks are fixtures
  byte for byte (a test enforces it). Table snippets are placed in minimal
  context; doc ellipses `…` are filled and `a | b` alternatives (notation,
  not syntax) become separate lines.

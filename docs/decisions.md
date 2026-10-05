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
  Because `0 -2px 8px` looks like three terms, a `+`/`-` in a
  space-separated value with a space before and none after is an error
  (`syntax::ambiguous_sign`) offering `(-2px)` or `0 - 2px`: loud, not a
  silently different shadow. `bg: f (x)` warns that it is two values.
- **2026-10-05 · Value precedence.** In a prop value: `~` loosest, then `,`,
  then space separation, then expression operators. So
  `lg: 0 8px 24px $a, 0 1px 2px $b` is a two-shadow list and
  `margin: 8, 8, 0 ~ instant` springs the whole value.
- **2026-10-05 · Line structure.** Newlines are ignored in `()`/`[]`,
  significant in `{}`. A line continues only if it starts with a binary
  operator, `.`, `?.`, `?`, `:`, `~`, `=>`, `<->` or `else`, or the previous
  line ended with a binary operator, `,`, `=`, `=>`, `<->`, `~` or a
  ternary's `?`/`:`. A line ending in `.`/`?.` or a prop's `:` does *not*
  continue: it is an error at the end of the line and the next line is its
  own item (props end at a line break; the LSP completes there). A line
  starting with `-` touching its operand (`-1 => b`, `-x.f()`) is a new
  item; `- x` continues. Optional blocks (element bodies, prop sub-blocks)
  must open on the head's line; mandatory ones may open on the next.
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
- **2026-10-05 · Nesting limit 128, tree depth 256.** Recursive nesting
  (blocks, brackets, prefix operators) is capped at 128. Flat chains
  (`a + b + …`, `a.b.c`, `else if`) are parsed in loops and only count
  toward a tree-depth cap of 256, so a 200-link chain in a few blocks is
  fine. Measured: a 256-deep tree parses, dumps and drops on a 2 MiB
  debug stack; 512 does not. Later passes that recurse over the tree can
  rely on depth ≤ 256.
- **2026-10-05 · Missing `}` recovery.** A top-level declaration keyword at
  column 0 inside an open block (also an `enum` body or `match` arms, where
  `let`/`state` count too) closes the open blocks with one "unclosed `{`"
  error, and a `}` indented unlike its `{` *inside the unclosed block*
  (the latest one) marks the likely culprit, so the error points at the
  right line (design.md "What you see" #3).
- **2026-10-05 · Config file discovery is one function.**
  `strand_compiler::source::find_files`: `.strand` files up to three
  directories down (the watcher's depth), symlinks followed, names starting
  with `.` skipped (files and directories), files deduplicated by canonical
  path (design.md: Strand canonicalises each loaded file), unreadable
  sub-directories reported and skipped. A file argument checks that file.
  `strand check` and the watcher use it, so they load the same set. Until
  the checker lands `strand check` reports syntax diagnostics only.
- **2026-10-05 · Diagnostics live at `strand_compiler::diagnostic`,** not
  under `syntax`, because the checker and reconciler will share them. Each
  label carries a `FileId` (into a `SourceMap`) and a file-local `Span`, so
  one diagnostic can point at two files (a name declared twice). Rendering
  draws at most 50 diagnostics per file, then "and N more".
- **2026-10-05 · Lambda parameters take no defaults.** `(a: int, b) => …`;
  a lambda is always called with every argument, so `= default` would add
  a concept for nothing.
- **2026-10-05 · Misspelt tree keywords.** `whn hover { … }` and
  `enterr { … }` are valid element syntax, so the parser cannot flag them
  without the element list. The wave-2 checker's unknown-element
  did-you-mean must include the tree and top-level keywords as candidates
  (TODO for `check`). `stat x = 0` (an element followed by more than an
  element holds) is flagged by the parser; at the top level it is one
  error.
- **2026-10-05 · Transitions and poses on `if` branches and pages.**
  design.md puts `transition: wipe(left)` "on `if`, `pages` and image
  swaps", but `if` has no prop position. A branch's transition or pose is
  written on the branch's root node (`if open { box { transition: wipe(left)
  } }`); for `pages`, on the `pages` element.
- **2026-10-05 · Keyframe stops** are percentages followed by a block, comma
  separated for shared stops: `keyframes shake { 0%, 100% { x: 0 }; 25% { x:
  -4 } }`.
- **2026-10-05 · A leading UTF-8 byte-order mark is trivia.**
- **2026-10-05 · Snippet fixtures.** design.md's code blocks are fixtures
  byte for byte (a test enforces it). Table snippets are placed in minimal
  context; doc ellipses `…` are filled and `a | b` alternatives (notation,
  not syntax) become separate lines.

# The `.strand` grammar

This is the complete syntax of `.strand` files. `design.md` is the source of
truth for what the constructs *mean*; this file fixes how they are *written*.
`strand-compiler::syntax` implements exactly this grammar, and every code
block in `design.md` parses under it with zero errors (see
`crates/strand-compiler/tests/fixtures/`).

Semantic restrictions that need names or types (which element kinds exist,
whether a prop is valid on a node, whether `slot` is inside a component) are
the checker's job, not the grammar's. Where the parser deliberately accepts
more than is meaningful so the checker can give a better message, this file
says so.

## Notation

EBNF: `'lit'` is a literal token, `A?` optional, `A*` zero or more, `A+` one
or more, `( … )` grouping, `|` alternatives. Upper-case names (`IDENT`,
`NUMBER`, …) are tokens. `kw:name` is an identifier token whose text is
`name` used as a keyword (see "Keywords are contextual").

Three pseudo-tokens describe line structure (see "Lines and termination"):

- `SAME_LINE` asserts that the next token is on the same line as the previous
  one. It consumes nothing.
- `SEP` is an item separator: one or more `;`, or a line break.
- `TOUCH` asserts that the next token directly follows the previous one,
  with no whitespace or comment between them.

## Lexical structure

The lexer is **lossless**: every byte of the file belongs to exactly one
token, including whitespace, line breaks, comments and invalid characters,
so concatenating the token texts reproduces the file. Each token carries its
byte span.

### Trivia

| Token | Form |
| --- | --- |
| whitespace | one or more of space, tab, `\r` |
| newline | `\n` (a `\r\n` pair is whitespace `\r` then newline) |
| comment | `//` up to, not including, the next `\n` |

There are no block comments. Trivia is skipped by the parser, except that
newlines are significant as described below.

### Identifiers and keywords

```
IDENT = [A-Za-z_] [A-Za-z0-9_]*
```

Identifiers are ASCII. `-` is never part of an identifier (it is minus), so
names are snake\_case (`max_width`, `time_left`). Types, components and
surfaces are conventionally PascalCase (`Toast`, `Pin`); the grammar does not
care. `_` alone is an ordinary identifier, used as the wildcard pattern.

#### Keywords are contextual

There are no reserved words. A word acts as a keyword only in the position
listed here; everywhere else it is an identifier. In particular, **an
identifier directly followed by `:` at the start of an item is always a prop
name** (`enter: popin(0.8)`, `type: password`, `text: <-> query`).

| Position | Words |
| --- | --- |
| Start of a top-level item | `component` `bar` `panel` `osd` `lock` `state` `let` `export` `enum` `type` `fn` `tokens` `use` `service` `permit` `keyframes` `on` `after` `every` |
| Start of a tree item | `when` `if` `match` `for` `on` `after` `every` `enter` `exit` `state` `let` `slot` `set` (only before `{`) `play` |
| Start of a handler statement | `let` `if` `match` `for` `play` |
| Start of an expression | `true` `false` `null` `match` `await` |
| Inside a construct | `else` `in` `key` `persist` `from` `extends` `override` `palette` `while` `change` `after` `rw` `system` `session` `every` |

### Token references, attributes and hash words

```
DOLLAR = '$' IDENT          // $accent, $space, $Toast
AT     = '@' IDENT          // @reset
HASH   = '#' W ( '-' W )*   where W = [A-Za-z0-9_]+
```

There is no whitespace inside these tokens. A `HASH` is a hex colour in
expression position (`#7aa2f7`; 3, 4, 6 or 8 hex digits) and an SVG element
selector at the start of a tree item (`#needle { … }`).

### Numbers and units

```
NUMBER = DIGITS ( '.' DIGITS )? UNIT?
DIGITS = [0-9]+
UNIT   = 'px' | '%' | 'deg' | 'ch' | 's' | 'ms'
```

- The fraction is taken only when `.` is followed by a digit, so `1.max(2)`
  is `1 . max(2)`.
- A number written directly after a `.` (no space) never takes a fraction:
  `$space.2` is a path segment `2`, and `x.0.1` is two segments.
- There are no leading-dot numbers: write `0.5`, not `.5`.
- A unit must touch the digits. Letters that touch the digits but are not a
  unit are a lexical error with a did-you-mean (`12pz` → `px`).
- `%` touching the digits is the percent unit (`40%`, `8%`); `%` after a
  space is the remainder operator (`a % b`).
- Negative numbers are unary minus applied to a number (`-1`, `months: -1`).

### Strings

```
STRING = '"' ( CHAR | ESCAPE )* '"'
ESCAPE = '\' ( '"' | '\' | 'n' | 't' | 'r' | '0' | 'u{' HEX+ '}' )
```

Strings hold any UTF-8 and may not span lines. There is no interpolation
(`design.md`: no mini-languages inside strings); use functions such as
`join(" · ", a, b)`. An unterminated string ends at the end of its line and is
an error.

### Punctuation and operators

Longest match wins.

```
{ } ( ) [ ] , ; : .
?.  ??  ?                 // ?. is not taken before a digit: a ?.5 is a ? .5
=>  ->  <->  ~
=  ==  !=  <  <=  >  >=  &&  ||  !
+  -  *  /  %  +=  -=  *=  /=
```

A lone `&` or `|` is an error with a hint (`&&`, `||`). Any other character
outside a string or comment is an invalid-character error token.

## Lines and termination

Newlines end things; brackets and operators let lines continue. The rules,
in full:

1. **Brackets.** Inside `( … )` and `[ … ]` newlines are ignored entirely.
   Inside `{ … }` (blocks, `match` bodies, lambda bodies) they are
   significant again, even when the braces sit inside parentheses.

2. **Items end at `;` or a line break.** Props, elements, declarations and
   statements in a block are separated by `SEP`: one or more `;` or a line
   break. Several items may share a line with `;`
   (`edge: top; height: 36`). An item that ends with a closing `}` may be
   followed by another item on the same line without a `;`.

3. **An expression continues onto the next line only when that line starts
   with a continuation token**:

   ```
   ??  ||  &&  ==  !=  <  <=  >  >=  +  -  *  /  %
   .  ?.  ?  :  ~  =>  <->  else
   ```

   ```
   wallpaper => material(image: prefs.wallpaper, variant: tonal_spot, dark: dark)
                ?? material(seed: prefs.accent, dark: dark),
   ```

   Only `-` can also start an item (a negative pattern `-1 => …`, a handler
   statement `-x.f()`). So **a line starting with `-` that touches its
   operand (`-1`, `-y`) starts a new item**; `- 1` with a space after the
   sign continues the line as subtraction. With that rule no continuation
   token is ambiguous.

4. **A line that ends with a binary operator, `,`, `=`, `=>`, `<->`, `~`
   or a ternary's `?` or `:` also continues**, because those tokens need a
   right-hand side and the parser simply reads it from the next line.

5. **Everything else stops at a line break.** In particular a call's `(`, an
   index's `[`, a positional argument, a space-separated value term and an
   optional block's `{` must be on the same line as what precedes them.
   Two cases are errors rather than continuations, because a half-typed line
   is exactly where an editor completes:
   - a `.` or `?.` ending a line: `text audio.` reports "expected a field
     name", and the next line is its own item;
   - a prop's or token's `:` ending a line: `bg:` reports "`bg:` has no
     value" (props end at a line break, `design.md`), and the next line is
     its own item.

6. **Mandatory `{` may follow a line break.** Where a block is required
   (declarations, `when`, `if`, `for`, `match`, `on`, `after`, `every`,
   `enter`, `exit`, `set`, selectors, `else`), it may start on the next line.
   Where a block is optional (an element's body, a prop's sub-block), `{` must
   be on the head's line; otherwise the line break ends the item.

7. **Clause keywords stay on the line** of what they extend: `key` (in `for`
   and keyed `state`), `persist`, `while`, `after` (in `on change`),
   `extends`, `every` (in `poll` services), `rw`. The only clause keyword
   that may start a new line is `else` (rule 3).

## Files

```
file      = SEP? ( top_item ( SEP top_item )* )? SEP? EOF
top_item  = attribute* top_decl
attribute = AT ( TOUCH '(' args? ')' )?      // @reset
top_decl  = component | surface | state | let | export | enum_decl | type_decl
          | fn_decl | tokens_decl | use_decl | service | permit | keyframes
          | on_handler | timer
```

There are no imports and no entry file; every `.strand` file in the config
directory is one module of the same program. Elements and props at the top
level are an error ("must be inside a surface or component"); the parser
reports it and parses them anyway to keep going.

## Declarations

### Components

```
component = kw:component IDENT params? component_tokens? tree_block
component_tokens = kw:tokens token_block
params    = '(' ( param ( ',' param )* ','? )? ')'
param     = IDENT ( ':' type )? ( '=' expr )?
```

```
component Toast(n: Notification, dense: bool = false) { … }
component Toast(n) tokens { radius: $radius.lg } { col { radius: $Toast.radius } }
component Clock { … }
```

Parameter types are optional in the grammar (the design writes
`component Toast(n) tokens { … }`); the checker requires them where they
cannot be inferred. Children passed at a call site render at `slot`.

### Surfaces

```
surface      = surface_kind IDENT tree_block
             | kw:lock IDENT? tree_block
surface_kind = kw:bar | kw:panel | kw:osd
```

`bar Top { … }` is on every monitor with `screen` in scope. `popup` is not a
top-level surface: it is an element anchored to its parent
(`popup { open: <-> open; Calendar }`).

### State, let and export

```
export = kw:export ( state | let )
state  = kw:state IDENT ( ':' type )? ( SAME_LINE kw:key expr )? '=' expr ( SAME_LINE kw:persist )?
       | kw:state IDENT kw:from STRING field_block
let    = kw:let IDENT ( ':' type )? '=' expr
field_block = '{' SEP? ( field ( SEP field )* )? SEP? '}'
field  = IDENT ':' type ( SAME_LINE kw:rw )? ( SAME_LINE '=' expr )?
```

```
state open = false
export state dnd = false persist
state pins: [Pin] key app = []
state prefs from "prefs.toml" { accent: color = #7aa2f7; compact: bool = false }
let scheme: Palette = match look { … }
```

`state` and `let` may also appear in tree blocks (state lives on components,
surfaces and list items); `let` also appears in handler and `fn` blocks as a
local binding.

### Enums and record types

```
enum_decl = kw:enum IDENT '{' SEP? ( IDENT ( ( ',' | SEP ) IDENT )* ','? )? SEP? '}'
type_decl = kw:type IDENT field_block
```

```
enum Kind { volume, brightness }
type Pin { app: AppId; label: text }
```

Records are built with call syntax and named arguments: `Pin(app: a, label:
"x")`. There is no separate record-literal form.

### Functions

```
fn_decl = kw:fn IDENT params ( '->' type )? stmt_block
```

`fn`s are pure; the value of the body is its last statement, which must be
an expression (the checker enforces purity and the final expression).

```
fn clamp01(x: float) -> float { x < 0 ? 0 : x > 1 ? 1 : x }
```

### Tokens

```
tokens_decl = kw:tokens IDENT ( SAME_LINE kw:extends IDENT )? token_block
token_block = '{' SEP? ( token_entry ( SEP token_entry )* )? SEP? '}'
token_entry = kw:override? token_key ( ':' value | token_block )
token_key   = ( DOLLAR | IDENT | INT ) ( '.' ( IDENT | INT ) )*
use_decl    = kw:use use_clause ( ',' use_clause )*
use_clause  = ( kw:tokens | kw:palette ) expr
```

`INT` is a `NUMBER` without fraction or unit. A key may carry the `$` it is
read with (`$surface.hi: …`) or not (`surface.hi: …`); both name the same
token. A `token_block` after a key is a group (`space { 1: 4px; 2: 8px }`).
The same `token_block` is used by `set { … }` and component `tokens { … }`.

```
tokens base {
  space  { 1: 4px; 2: 8px }
  font   { ui: "Inter" 13px 500 }
  elevation { lg: 0 8px 24px $shadow.alpha(0.3), 0 1px 2px $shadow.alpha(0.2) }
  surface.hi: $surface.mix($fg, 8%)
}
tokens compact extends base { override space { 1: 2px } }
use tokens prefs.compact ? compact : base, palette scheme
```

The comma in `use` separates clauses, so the ternary ends before it.

### Services and permits

```
service = kw:service IDENT kw:from source service_block
source  = IDENT source_arg* ( SAME_LINE kw:every expr )?
source_arg = SAME_LINE expr           // up to `{` or `every`
service_block = '{' SEP? ( service_item ( SEP service_item )* )? SEP? '}'
service_item  = field | permit
permit  = kw:permit IDENT ( SAME_LINE expr ( ',' expr )* )?
```

The source kind is one of `dbus`, `file`, `listen`, `poll` (others are an
error with a did-you-mean). Their arguments:

| Source | Arguments |
| --- | --- |
| `dbus` | `system` or `session`, the bus name string, optionally an object path string |
| `file` | one expression: the path |
| `listen` | one expression: the command (string or array) |
| `poll` | one expression: the command, then `every` and an interval |

```
service ppd from dbus system "net.hadess.PowerProfiles" { profile: text rw = ActiveProfile }
service temp from poll ["sensors", "-j"] every 5s { cpu: float = package }
permit exec "sensors"
```

`permit exec` (with an optional list of allowed programs) may appear at the
top level or inside a service block. Without it, `listen` and `poll` sources
are a load error.

### Keyframes and play

```
keyframes = kw:keyframes IDENT '{' SEP? ( kf_item ( SEP kf_item )* )? SEP? '}'
kf_item   = NUMBER ( ',' NUMBER )* tree_block   // percentages
          | prop
play      = kw:play expr
```

```
keyframes shake { 0%, 100% { x: 0 }; 25% { x: -4 }; 75% { x: 4 }; duration: 300ms }
on click { play shake }
```

`play` is accepted both in handlers (play once on the enclosing node) and as
a tree item; the checker decides what a tree-level `play` means.

## Tree blocks

The body of a surface, component, element, `when`, `if`, `for`, pose,
selector or prop sub-block.

```
tree_block = '{' SEP? ( tree_item ( SEP tree_item )* )? SEP? '}'
tree_item  = attribute* ( prop | element | when | if_tree | match_tree | for_tree
           | on_handler | timer | pose | slot | set | selector | play | state | let )
```

An item starting with an identifier is classified by its first two tokens:

1. `IDENT ':'` → prop.
2. A tree keyword in its keyword position (table above) → that construct.
3. Otherwise → element.
4. `HASH` → selector. Anything else is an error.

### Props

```
prop       = IDENT ':' ( '<->' expr | value ) transition? ( SAME_LINE tree_block )?
transition = '~' expr
value      = spaced ( ',' spaced )*          // comma shorthand
spaced     = expr ( SAME_LINE term_start expr )*   // space-separated group
```

Precedence inside a prop value, loosest first: `~`, then `,`, then
space-separation, then ordinary expression precedence. So:

| Written | Parsed as |
| --- | --- |
| `margin: 8, 8, 0` | shorthand of three values |
| `border: 1, $border` | shorthand of two values |
| `glow: 10 * wave(2s), $accent.alpha(0.4)` | shorthand of `10 * wave(2s)` and `$accent.alpha(0.4)` |
| `lg: 0 8px 24px $a, 0 1px 2px $b` | shorthand of two space-separated groups (a shadow list) |
| `font: "Inter" 13px 500` | one space-separated group |
| `width: 24 ~ $motion.bouncy` | value `24`, transition `$motion.bouncy` |
| `margin: 8, 8, 0 ~ instant` | the transition applies to the whole value |
| `value: <-> audio.sink.volume` | two-way binding to a place |

- A space-separated group continues only with a term that **starts** a new
  expression (`IDENT`, `DOLLAR`, `NUMBER`, `STRING`, `HASH`, `(`, `[`) on the
  same line, and never with an identifier followed by `:` (that is a missing
  `;`) or with a tree keyword.
- **Operators always bind.** `-` between two terms is subtraction, never the
  sign of the next term, so whitespace never changes meaning (`design.md`,
  "Bad"). Because `0 -2px 8px` *looks* like three terms, a `+` or `-` in a
  space-separated value with a space before it and none after is an error
  (`syntax::ambiguous_sign`) whose help offers both readings: `(-2px)` for a
  negative term, `0 - 2px` to subtract. Write a negative term in
  parentheses: `0 (-2px) 8px $shadow`.
- Parentheses after a space are a new term, not a call (see "Calls touch").
  When the term before them is a name (`bg: f (x)`), a warning
  (`syntax::spaced_call`) says it is two values and how to call `f`.
- `<->` takes a single expression: no commas, no spaces.
- A prop may carry a sub-block of props for structured values:
  `stroke: 3, $accent { trim: 0, progress; wave: 2, 18px; cap: round }`.

### Elements: one call shape

```
element  = IDENT head_arg? ( SAME_LINE tree_block )?
head_arg = SAME_LINE ( IDENT ':' expr | expr )
```

`kind [positional] { props; children }` for built-in elements and components
alike: `text clock.format("%H:%M")`, `Dot ws`, `Toast n { dense: true }`,
`Volume`, `merge 10 { … }`, `shader "aurora.wgsl" { u_speed: 0.4 }`.

**Positional-then-block.** The positional is one expression, and an
expression never consumes a `{` except as the body of a `match` or directly
after a lambda's `=>`. So in `text windows.focused?.title ?? "" { color: $fg }`
the first `{` at the expression's top level opens the element's block. To
use a lambda with a block body as a positional, wrap it in parentheses.

The head argument may be written in named form `name: expr`, which is sugar
for a first prop: `pages current: page { page wifi { … } }` is
`pages { current: page; page wifi { … } }`.

### Conditional style, structure and loops

```
when     = kw:when expr tree_block
if_tree  = kw:if expr tree_block ( kw:else ( if_tree | tree_block ) )?
for_tree = kw:for IDENT kw:in expr ( SAME_LINE kw:key expr )? tree_block
match_tree = kw:match expr '{' SEP? ( tree_arm ( ( ',' | SEP ) tree_arm )* ','? )? SEP? '}'
tree_arm = pattern '=>' ( tree_block | tree_item )
```

`when hover { bg: $accent.hover }`, `if battery.present { Battery }`,
`for ws in workspaces.on(screen) { Dot ws }`,
`for p in prefs.pins key p.app { … }`. A `when` block holds props only; the
parser accepts any tree item there and the checker reports children.

### Events, timers and poses

```
on_handler = kw:on ( kw:change change_targets ( SAME_LINE kw:after expr )?
                   | IDENT ( '.' IDENT )* params? ) stmt_block
change_targets = expr ( ',' expr )*
timer = ( kw:after | kw:every ) expr ( SAME_LINE kw:while expr )? stmt_block
pose  = ( kw:enter | kw:exit ) tree_block
slot  = kw:slot
set   = kw:set token_block
selector = HASH tree_block
```

```
on click { ws.focus() }
on scroll(dy) { audio.sink.volume -= dy * 0.05 }
on drop(p: Pin, at: int) { pins.move(p.app, at) }
on notifications.received(n) { log.push(n) }
on change audio.sink.volume, audio.sink.muted after 1.2s { shown = false }
after n.timeout ?? 6s while !hover && n.urgency != critical { n.expire() }
every 1s while visible { tick += 1 }
enter { width: 0; opacity: 0 }
set { $surface: $surface.alpha(0.5) }
svg "icon.svg" { #needle { rotate: level * 270deg } }
```

`on change` takes one or more watched expressions; the comma separates them,
so they end before `after` or `{`. `change` directly followed by `(` or `{`
is an ordinary event name (`on change(x) { … }` declares a parameter), and
so is `change` followed by `.` (an event path such as `on change.done`), so
the watched expressions are never wrapped in parentheses. Timers and `on` handlers are allowed at
the top level (the OSD example) and in tree blocks.

## Handler blocks

The body of `on`, `after`, `every`, a `fn`, and a lambda's block body.

```
stmt_block = '{' SEP? ( stmt ( SEP stmt )* )? SEP? '}'
stmt  = let
      | kw:if expr stmt_block ( kw:else ( stmt_if | stmt_block ) )?
      | kw:match expr '{' SEP? ( stmt_arm ( ( ',' | SEP ) stmt_arm )* ','? )? SEP? '}'
      | kw:for IDENT kw:in expr ( SAME_LINE kw:key expr )? stmt_block
      | play
      | expr ( assign_op expr )?
stmt_arm  = pattern '=>' ( stmt_block | stmt )
assign_op = '=' | '+=' | '-=' | '*=' | '/='
```

`open = !open`, `kind = volume; shown = true`, `h.app.launch(); open = false`,
`audio.sink.volume -= dy * 0.05`. Assignment is a statement, never an
expression. The assignee must be a place (name, field path or index); the
checker rejects `let`s and bound props.

## Expressions

```
expr     = lambda | ternary
lambda   = ( IDENT | '(' ( lparam ( ',' lparam )* ','? )? ')' ) '=>' ( stmt_block | expr )
lparam   = IDENT ( ':' type )?          // no defaults: a lambda is called with every argument
ternary  = coalesce ( '?' expr ':' expr )?
coalesce = or ( '??' coalesce )?
or       = and ( '||' and )*
and      = equality ( '&&' equality )*
equality = compare ( ( '==' | '!=' ) compare )*
compare  = additive ( ( '<' | '<=' | '>' | '>=' ) additive )?
additive = multiplicative ( ( '+' | '-' ) multiplicative )*
multiplicative = unary ( ( '*' | '/' | '%' ) unary )*
unary    = ( '!' | '-' | kw:await ) unary | postfix
postfix  = primary ( ( '.' | '?.' ) SAME_LINE ( IDENT | INT ) | TOUCH '(' args? ')' | TOUCH '[' expr ']' )*
primary  = NUMBER | STRING | HASH | kw:true | kw:false | kw:null
         | token_path | IDENT | '(' expr ')' | '[' ( expr ( ',' expr )* ','? )? ']'
         | match_expr
token_path = DOLLAR ( '.' ( IDENT | INT ) )*       // a segment followed by TOUCH '(' is a method
match_expr = kw:match expr '{' SEP? ( expr_arm ( ( ',' | SEP ) expr_arm )* ','? )? SEP? '}'
expr_arm   = pattern '=>' expr
args     = arg ( ',' arg )* ','?
arg      = IDENT ':' expr          // named: month.add(months: -1)
         | kw:from expr            // relative colour: oklch(from $surface, l: l + 0.12)
         | expr
```

Precedence, loosest first:

| Level | Operators | Associativity |
| --- | --- | --- |
| 1 | `x => e` lambda | right |
| 2 | `c ? a : b` | right |
| 3 | `??` | right |
| 4 | `\|\|` | left |
| 5 | `&&` | left |
| 6 | `==` `!=` | left |
| 7 | `<` `<=` `>` `>=` | none |
| 8 | `+` `-` | left |
| 9 | `*` `/` `%` | left |
| 10 | `!` `-` `await` (prefix) | — |
| 11 | `.f` `?.f` `f(…)` `x[i]` (postfix) | left |

Notes:

- **Calls touch.** `TOUCH` means no whitespace between the callee and `(` or
  `[`, as with CSS function tokens: `pct(x)` is a call, `pct (x)` is two
  terms. Outside a prop value two terms in a row are an error whose help says
  to remove the space.
- **Token paths.** `$space.2`, `$radius.lg`, `$fg.muted`, `$Toast.radius` are
  paths. A segment followed by a touching `(` is a method call on the path
  before it (`$surface.alpha(0.72)`, `$fg.mix($bg, 8%)`), so adding methods
  later never breaks token names.
- **`?.`** short-circuits to null; `??` replaces null and a pending or failed
  `Async` (`windows.focused?.title ?? ""`).
- **Named arguments** are `IDENT ':'` at the start of an argument. A ternary
  argument (`f(a ? b : c)`) never starts with `IDENT ':'`.
- **`from`** at the start of an argument and not followed by `:` is the
  relative-colour source (`oklch(from $surface, l: l + 0.12)`); `from:` is an
  ordinary named argument (`conic(from: 90deg, …)`).
- **Lambdas.** `n => !dnd || n.urgency == critical`, `(c) => …`,
  `(a: int, b) => { … }`. A parenthesised list is a lambda only when its
  closing `)` is followed by `=>`.
- **`match`** as an expression has one expression per arm, separated by
  commas or line breaks; an arm continues onto the next line only by rule 3.
- **`await`** is accepted in any expression; the checker allows it only in
  handlers, which are coroutines.

### Literals

| Literal | Examples |
| --- | --- |
| Integers, decimals | `36`, `0.72` |
| Lengths | `4px`, `40%`, `4ch` |
| Angles | `270deg` |
| Durations | `6s`, `1.2s`, `200ms` |
| Colours | `#7aa2f7`, `#fff`, `#00000080` |
| Strings | `"Search apps"`, `"‹"` |
| Booleans, null | `true`, `false`, `null` |
| Arrays | `[]`, `["a", "b"]` |

Enum variants (`critical`, `volume`, `top_right`) and names in scope are
plain identifiers; the checker resolves them by expected type.

## Types, patterns, parameters

```
type    = type_atom ( TOUCH '?' )*
type_atom = '[' type ']'                      // [Pin]
          | IDENT ( '.' IDENT )* ( '<' type ( ',' type )* '>' )?   // Async<[Hit]>
pattern = IDENT ( '.' IDENT )*                // a variant, or `_`
        | '-'? NUMBER | STRING | HASH | kw:true | kw:false | kw:null
```

A negative pattern on its own line (`-1 => …`) is a new arm, by rule 3.

`_` is the wildcard pattern.

## Error recovery

The parser never panics and always returns a full `File`. It records a
diagnostic and resynchronises:

- In a block, an item that fails skips to the next `;`, line break or
  closing `}` at its own brace depth, then continues with the next item.
- An expression that cannot start yields an error node without consuming
  the token, so the enclosing list or block decides how to recover.
- A missing `}` is reported once, at the unclosed `{`, when the file ends
  or when a top-level declaration keyword (`component`, `bar`, `enum`,
  `tokens`, …, and in `match` arms and `enum` variants also `let` and
  `state`) appears at column 0; every enclosing block closes there and the
  declaration parses normally. A `{` inside the unclosed block whose `}` was
  indented differently from its opening line is named as the likely culprit.
- Nesting deeper than 128 levels (blocks, brackets, prefix operators,
  operands) is an error, never a stack overflow. Flat chains (`a + b + c`,
  `a.b.c`, `else if`) are parsed in loops, but each link deepens the tree,
  so the tree as a whole may be at most 256 levels deep; past that is one
  `syntax::too_deep` error. A per-file step budget guarantees termination.
- Unknown words where a keyword was expected get a did-you-mean from the
  keywords valid at that position (`componnet` → `component`,
  `on chnage a` → `change`, `for x im xs` → `in`, `dbsu` → `dbus`).

## Constructs by design section

| Design construct | Production |
| --- | --- |
| `bar Top { edge: top }` | `surface` |
| `Dot ws`, `Toast n { dense: true }` | `element` |
| `component Toast(n: Notification, dense: bool = false)`, `slot` | `component`, `slot` |
| `margin: 8, 8, 0`, `font: "Inter" 13px 500`, shadow lists | `prop`, `value` |
| `when hover { … }`, `vol.hover` | `when`, `postfix` |
| `if` / `else`, `match`, `for x in xs key e` | `if_tree`, `match_tree`, `for_tree`, `stmt` |
| `enter {}`, `exit {}` | `pose` |
| `state x = 0 persist`, `state p from "f.toml" { … }`, `let`, `export` | `state`, `let`, `export` |
| `enum`, `type`, `state xs: [T] key f = []` | `enum_decl`, `type_decl`, `state` |
| `on click`, `on scroll(dy)`, `on change a, b after T` | `on_handler` |
| `after T while c { }`, `every T while c { }` | `timer` |
| `prop: <-> target` | `prop` |
| `width: 24 ~ $motion.bouncy` | `transition` |
| `$space.2`, `$fg.alpha(0.25)` | `token_path`, `postfix` |
| `?.`, `??`, `?:`, `x => e`, `f(months: -1)` | `expr` |
| `tokens base { … }`, `extends`, `override`, `use tokens …, palette …` | `tokens_decl`, `use_decl` |
| `set { $surface: … }` | `set` |
| `service … from dbus system "…" { f: text rw = P }`, `permit exec` | `service`, `permit` |
| `fn`, `keyframes`, `play` | `fn_decl`, `keyframes`, `play` |
| `shader "x.wgsl" { … }`, `canvas { draw: (c) => … }` | `element` |
| `oklch(from $surface, l: l + 0.12)` | `arg` |
| `svg "x.svg" { #needle { … } }` | `selector` |
| `@reset` | `attribute` |

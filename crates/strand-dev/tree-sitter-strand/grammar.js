/**
 * tree-sitter grammar for `.strand` files.
 *
 * docs/grammar.md is the syntax and docs/spec.md section 3 the list of
 * constructs; this grammar follows them for editors (highlighting,
 * folding, indentation). The compiler's parser (strand-compiler::syntax)
 * stays the language: crates/strand-dev/tests/tree_sitter.rs holds this
 * grammar to it on every fixture, repo `.strand` file and guide block.
 *
 * How grammar.md's pseudo-tokens are written here (decisions.md, m5-lsp):
 * - SEP is `;` or a line break. The external scanner (src/scanner.c)
 *   turns a line break into a separator only where the parser can take
 *   one, and not when the next line starts with a continuation token
 *   (grammar.md, "Lines and termination", rule 3). Everywhere else a line
 *   break is whitespace, so brackets and dangling operators continue a
 *   line (rules 1 and 4).
 * - SAME_LINE falls out of SEP: where an item may end, a line break ends it.
 * - TOUCH is `token.immediate`: a call's `(`, an index's `[`, a type's `?`.
 * - Keywords are contextual: `word` makes every keyword a keyword only in
 *   a state that can take it, and the places where a keyword may also be
 *   a name (a prop, a named argument, an event) alias it to `identifier`.
 * - An item that ends with a block needs no separator before the next one
 *   on its line (rule 2), so lists of items come in two kinds of item.
 *   An item whose production does not end in a block may still end in a
 *   `}` (a `match` expression, a lambda's block): the scanner lexes `}`
 *   and, when the next token on the line starts a new item, gives the
 *   zero-width separator the parser's "the previous token closed a block"
 *   stands for.
 */

const PREC = {
  lambda: 1,
  ternary: 2,
  coalesce: 3,
  or: 4,
  and: 5,
  equality: 6,
  compare: 7,
  additive: 8,
  multiplicative: 9,
  unary: 10,
  postfix: 11,
};

// Words that start a construct in a tree (grammar.md's keyword table): a
// prop may still be named by one (`enter: popin(0.8)`, `exit: …`).
const TREE_KEYWORDS = [
  'when', 'if', 'match', 'for', 'on', 'after', 'every', 'enter', 'exit',
  'state', 'let', 'slot', 'set', 'play',
];

/**
 * Items in a block: one that ends with a block (`blocky`) may be followed
 * by another on the same line; any other (`bare`) needs a separator. With
 * no `blocky`, every item needs one.
 */
function items($, bare, blocky) {
  if (!blocky) {
    return seq(
      optional($._sep),
      optional(seq(bare, repeat(seq($._sep, bare)), optional($._sep))),
    );
  }
  return seq(
    optional($._sep),
    repeat(choice(seq(bare, $._sep), seq(blocky, optional($._sep)))),
    optional(bare),
  );
}

/** `{ items }` */
function block($, bare, blocky) {
  return seq('{', items($, bare, blocky), '}');
}

/** One or more `rule`, separated by `,`, with an optional trailing one. */
function commaSep1(rule) {
  return seq(rule, repeat(seq(',', rule)), optional(','));
}

/** A list separated by `,` or SEP (enum variants, match arms). */
function armList($, rule) {
  return seq(
    '{',
    repeat($._list_sep),
    optional(seq(rule, repeat(seq(repeat1($._list_sep), rule)), repeat($._list_sep))),
    '}',
  );
}

module.exports = grammar({
  name: 'strand',

  word: $ => $.identifier,

  externals: $ => [
    // A line break that ends an item (rule 2), decided by src/scanner.c.
    $._line_break,
    // Never valid: tells the scanner the parser is recovering from an error.
    $._error_sentinel,
    // A block's closing brace, lexed by the scanner where it is valid so
    // that it knows when the previous token closed a block (rule 2).
    '}',
    // Never produced: valid where a prop value's next space-separated term
    // may follow, so a term after a `}` there continues the value.
    $._group_term,
  ],

  extras: $ => [/[ \t\r\f\n]/, $.comment],

  supertypes: $ => [$._expression],

  conflicts: $ => [
    // `(a)` is a parenthesised name until a `=>` makes it a lambda's
    // parameter list.
    [$._primary, $.lambda_parameter],
  ],

  rules: {
    source_file: $ => items($, $._top_bare, $._top_blocky),

    _sep: $ => prec.right(repeat1(choice($._line_break, ';'))),
    _list_sep: $ => choice(',', $._line_break, ';'),

    comment: _ => token(seq('//', /[^\n]*/)),

    // ---- Top level -------------------------------------------------------

    _top_bare: $ => choice(
      $.state,
      $.let,
      $.export,
      $.use,
      $.permit,
      alias($._top_file_bare, $.element),
    ),

    // An attribute needs no separator before the item it marks
    // (`@reset state q = ""`), so it is listed with the block items.
    _top_blocky: $ => choice(
      $.attribute,
      $.component,
      $.surface,
      alias($._state_from, $.state),
      $.enum,
      $.type,
      $.function,
      $.tokens,
      $.service,
      $.keyframes,
      $.on_handler,
      $.timer,
      alias($._top_file_block, $.element),
    ),

    attribute: $ => seq(
      field('name', $.attribute_name),
      optional($.arguments),
    ),
    attribute_name: _ => /@[A-Za-z_][A-Za-z0-9_]*/,

    // `file "path" { … }` at the top level (grammar.md's `top_file`).
    _top_file_bare: $ => seq(
      field('name', alias('file', $.identifier)),
      optional(field('argument', $._head)),
    ),
    _top_file_block: $ => seq(
      field('name', alias('file', $.identifier)),
      optional(field('argument', $._head)),
      field('body', $.tree_block),
    ),

    // ---- Declarations ----------------------------------------------------

    component: $ => seq(
      'component',
      field('name', $.identifier),
      optional(field('parameters', $.parameters)),
      optional(seq('tokens', field('tokens', $.token_block))),
      field('body', $.tree_block),
    ),

    surface: $ => choice(
      seq(
        field('kind', choice('bar', 'panel', 'osd')),
        field('name', $.identifier),
        field('body', $.tree_block),
      ),
      seq(
        field('kind', 'lock'),
        optional(field('name', $.identifier)),
        field('body', $.tree_block),
      ),
    ),

    parameters: $ => seq('(', optional(commaSep1($.parameter)), ')'),
    parameter: $ => seq(
      field('name', $.identifier),
      optional(seq(':', field('type', $._type))),
      optional(seq('=', field('default', $._expression))),
    ),

    state: $ => seq(
      'state',
      field('name', $.identifier),
      optional(seq(':', field('type', $._type))),
      optional(seq('key', field('key', $._expression))),
      '=',
      field('value', $._expression),
      optional('persist'),
    ),

    _state_from: $ => seq(
      'state',
      field('name', $.identifier),
      'from',
      field('file', $.string),
      field('fields', $.field_block),
    ),

    let: $ => seq(
      'let',
      field('name', $.identifier),
      optional(seq(':', field('type', $._type))),
      '=',
      field('value', $._expression),
    ),

    export: $ => seq('export', choice($.state, alias($._state_from, $.state), $.let)),

    field_block: $ => block($, $.field),
    field: $ => seq(
      field('name', $.identifier),
      ':',
      field('type', $._type),
      optional('rw'),
      optional(seq('=', field('default', $._expression))),
    ),

    enum: $ => seq(
      'enum',
      field('name', $.identifier),
      field('variants', alias($._variants, $.variant_list)),
    ),
    _variants: $ => armList($, alias($.identifier, $.variant)),

    type: $ => seq('type', field('name', $.identifier), field('fields', $.field_block)),

    function: $ => seq(
      'fn',
      field('name', $.identifier),
      field('parameters', $.parameters),
      optional(seq('->', field('return_type', $._type))),
      field('body', $.stmt_block),
    ),

    tokens: $ => seq(
      'tokens',
      field('name', $.identifier),
      optional(seq('extends', field('extends', $.identifier))),
      field('body', $.token_block),
    ),

    token_block: $ => block(
      $,
      alias($._token_entry_value, $.token_entry),
      alias($._token_entry_group, $.token_entry),
    ),
    _token_entry_value: $ => seq(
      optional('override'),
      field('key', $.token_key),
      ':',
      field('value', $.value),
    ),
    _token_entry_group: $ => seq(
      optional('override'),
      field('key', $.token_key),
      field('body', $.token_block),
    ),
    token_key: $ => seq(
      choice($.token, $.identifier, alias($._int, $.integer)),
      repeat(seq('.', choice($.identifier, alias($._int, $.integer)))),
    ),

    use: $ => seq('use', commaSep1NoTrail($.use_clause)),
    use_clause: $ => seq(field('kind', choice('tokens', 'palette')), field('value', $._expression)),

    service: $ => seq(
      'service',
      field('name', $.identifier),
      'from',
      field('source', $.source),
      field('body', $.service_block),
    ),

    source: $ => choice(
      seq(
        field('kind', 'dbus'),
        field('bus', choice('system', 'session')),
        field('name', $.string),
        optional(field('path', $.string)),
      ),
      seq(field('kind', choice('file', 'listen')), field('argument', $._expression)),
      seq(
        field('kind', 'poll'),
        field('argument', $._expression),
        'every',
        field('interval', $._expression),
      ),
    ),

    service_block: $ => block($, choice($.field, $.permit)),

    permit: $ => seq(
      'permit',
      field('kind', $.identifier),
      optional(seq($._expression, repeat(seq(',', $._expression)))),
    ),

    keyframes: $ => seq(
      'keyframes',
      field('name', $.identifier),
      field('body', alias($._keyframes_block, $.keyframes_block)),
    ),
    _keyframes_block: $ => block(
      $,
      alias($._prop_bare, $.prop),
      choice($.keyframe, alias($._prop_block, $.prop)),
    ),
    keyframe: $ => seq(
      field('offset', $.number),
      repeat(seq(',', field('offset', $.number))),
      field('body', $.tree_block),
    ),

    // ---- Trees -----------------------------------------------------------

    tree_block: $ => block($, $._tree_bare, $._tree_blocky),

    _tree_bare: $ => choice(
      alias($._prop_bare, $.prop),
      alias($._element_bare, $.element),
      $.slot,
      $.play,
      $.state,
      $.let,
    ),

    _tree_blocky: $ => choice(
      $.attribute,
      alias($._prop_block, $.prop),
      alias($._element_block, $.element),
      $.when,
      $.if_tree,
      $.match_tree,
      $.for_tree,
      $.on_handler,
      $.timer,
      $.pose,
      $.set,
      $.selector,
      alias($._state_from, $.state),
    ),

    _prop_name: $ => choice(
      $.identifier,
      alias(choice(...TREE_KEYWORDS), $.identifier),
    ),

    _prop_bare: $ => seq(
      field('name', $._prop_name),
      ':',
      $._prop_value,
    ),
    _prop_block: $ => seq(
      field('name', $._prop_name),
      ':',
      $._prop_value,
      field('body', $.tree_block),
    ),
    _prop_value: $ => seq(
      choice(field('binding', $.binding), field('value', $.value)),
      optional(field('transition', $.transition)),
    ),

    binding: $ => seq('<->', $._expression),
    transition: $ => seq('~', $._expression),

    // `a, b` (comma shorthand) of `x y z` (space-separated groups).
    // Greedy: in a match arm, `a => margin: 8, 8` keeps both values.
    value: $ => prec.right(seq(
      choice($._expression, $.group),
      repeat(seq(',', choice($._expression, $.group))),
    )),
    group: $ => prec.left(seq(
      $._expression,
      repeat1(prec.left(seq(optional($._group_term), $._expression))),
    )),

    _element_bare: $ => seq(
      field('name', $.identifier),
      optional(field('argument', $._head)),
    ),
    _element_block: $ => seq(
      field('name', $.identifier),
      optional(field('argument', $._head)),
      field('body', $.tree_block),
    ),
    _head: $ => choice($._expression, $.named_argument),

    when: $ => seq('when', field('condition', $._expression), field('body', $.tree_block)),

    if_tree: $ => seq(
      'if',
      field('condition', $._expression),
      field('consequence', $.tree_block),
      optional(seq('else', field('alternative', choice($.if_tree, $.tree_block)))),
    ),

    for_tree: $ => seq(
      'for',
      field('binding', $.identifier),
      'in',
      field('iterable', $._expression),
      optional(seq('key', field('key', $._expression))),
      field('body', $.tree_block),
    ),

    match_tree: $ => seq(
      'match',
      field('subject', $._expression),
      field('arms', alias($._tree_arms, $.match_arms)),
    ),
    _tree_arms: $ => armList($, alias($._tree_arm, $.match_arm)),
    _tree_arm: $ => seq(
      field('pattern', $.pattern),
      '=>',
      field('body', choice($.tree_block, $._tree_bare, $._tree_blocky)),
    ),

    on_handler: $ => seq(
      'on',
      choice(
        seq(
          'change',
          field('watch', $._expression),
          repeat(seq(',', field('watch', $._expression))),
          optional(seq('after', field('delay', $._expression))),
        ),
        seq(
          field('event', $.event_name),
          optional(field('parameters', $.parameters)),
        ),
      ),
      field('body', $.stmt_block),
    ),
    // `change` directly followed by `(`, `.` or `{` names the event.
    event_name: $ => prec(1, seq(
      choice($.identifier, alias('change', $.identifier)),
      repeat(seq('.', $.identifier)),
    )),

    timer: $ => seq(
      field('kind', choice('after', 'every')),
      field('interval', $._expression),
      optional(seq('while', field('condition', $._expression))),
      field('body', $.stmt_block),
    ),

    pose: $ => seq(field('kind', choice('enter', 'exit')), field('body', $.tree_block)),
    slot: _ => 'slot',
    set: $ => seq('set', field('body', $.token_block)),
    selector: $ => seq(field('name', alias($._hash, $.selector_name)), field('body', $.tree_block)),
    play: $ => seq('play', field('keyframes', $._expression)),

    // ---- Handlers --------------------------------------------------------

    stmt_block: $ => block($, $._stmt_bare, $._stmt_blocky),

    _stmt_bare: $ => choice(
      $.let,
      $.play,
      $.assignment,
      $.expression_statement,
    ),
    _stmt_blocky: $ => choice(
      $.if_statement,
      $.for_statement,
    ),

    assignment: $ => seq(
      field('left', $._expression),
      field('operator', choice('=', '+=', '-=', '*=', '/=')),
      field('right', $._expression),
    ),
    expression_statement: $ => $._expression,

    if_statement: $ => seq(
      'if',
      field('condition', $._expression),
      field('consequence', $.stmt_block),
      optional(seq('else', field('alternative', choice($.if_statement, $.stmt_block)))),
    ),
    for_statement: $ => seq(
      'for',
      field('binding', $.identifier),
      'in',
      field('iterable', $._expression),
      optional(seq('key', field('key', $._expression))),
      field('body', $.stmt_block),
    ),

    // ---- Expressions -----------------------------------------------------

    _expression: $ => choice(
      $.lambda,
      $.ternary_expression,
      $.binary_expression,
      $.unary_expression,
      $.member_expression,
      $.call_expression,
      $.index_expression,
      $._primary,
    ),

    _primary: $ => choice(
      $.number,
      $.string,
      alias($._hash, $.color),
      $.boolean,
      $.null,
      $.token,
      $.identifier,
      $.parenthesized_expression,
      $.array,
      $.match_expression,
    ),

    lambda: $ => prec.right(PREC.lambda, seq(
      field('parameters', choice(
        alias($.identifier, $.lambda_parameter),
        alias($._lambda_parameters, $.lambda_parameters),
      )),
      '=>',
      field('body', choice($.stmt_block, $._expression)),
    )),
    _lambda_parameters: $ => seq('(', optional(commaSep1($.lambda_parameter)), ')'),
    lambda_parameter: $ => seq(
      field('name', $.identifier),
      optional(seq(':', field('type', $._type))),
    ),

    ternary_expression: $ => prec.right(PREC.ternary, seq(
      field('condition', $._expression),
      '?',
      field('consequence', $._expression),
      ':',
      field('alternative', $._expression),
    )),

    binary_expression: $ => choice(
      prec.right(PREC.coalesce, seq(field('left', $._expression), field('operator', '??'), field('right', $._expression))),
      ...[
        ['||', PREC.or],
        ['&&', PREC.and],
        ['==', PREC.equality],
        ['!=', PREC.equality],
        ['<', PREC.compare],
        ['<=', PREC.compare],
        ['>', PREC.compare],
        ['>=', PREC.compare],
        ['+', PREC.additive],
        ['-', PREC.additive],
        ['*', PREC.multiplicative],
        ['/', PREC.multiplicative],
        ['%', PREC.multiplicative],
      ].map(([op, p]) => prec.left(p, seq(
        field('left', $._expression),
        field('operator', op),
        field('right', $._expression),
      ))),
    ),

    unary_expression: $ => prec(PREC.unary, seq(
      field('operator', choice('!', '-', 'await')),
      field('operand', $._expression),
    )),

    member_expression: $ => prec(PREC.postfix, seq(
      field('object', $._expression),
      choice('.', '?.'),
      field('field', choice(alias($.identifier, $.field_name), alias($._int, $.field_name))),
    )),

    call_expression: $ => prec(PREC.postfix, seq(
      field('function', $._expression),
      field('arguments', $.arguments),
    )),
    arguments: $ => seq(token.immediate('('), optional(commaSep1($._argument)), ')'),
    _argument: $ => choice($.named_argument, $.from_argument, $._expression),
    named_argument: $ => seq(
      field('name', choice($.identifier, alias('from', $.identifier))),
      ':',
      field('value', $._expression),
    ),
    from_argument: $ => seq('from', field('value', $._expression)),

    index_expression: $ => prec(PREC.postfix, seq(
      field('object', $._expression),
      token.immediate('['),
      field('index', $._expression),
      ']',
    )),

    parenthesized_expression: $ => seq('(', $._expression, ')'),
    array: $ => seq('[', optional(commaSep1($._expression)), ']'),

    match_expression: $ => seq(
      'match',
      field('subject', $._expression),
      field('arms', alias($._expr_arms, $.match_arms)),
    ),
    _expr_arms: $ => armList($, alias($._expr_arm, $.match_arm)),
    // One `match` for expressions and handler statements (grammar.md's
    // `match_expr` and `stmt`'s match): an arm's body may be a statement,
    // which the checker allows only in a handler.
    _expr_arm: $ => seq(
      field('pattern', $.pattern),
      '=>',
      field('body', choice(
        $._expression,
        $.stmt_block,
        $.assignment,
        $.let,
        $.play,
        $.if_statement,
        $.for_statement,
      )),
    ),

    pattern: $ => choice(
      seq($.identifier, repeat(seq('.', $.identifier))),
      seq(optional('-'), $.number),
      $.string,
      alias($._hash, $.color),
      $.boolean,
      $.null,
    ),

    // ---- Types -----------------------------------------------------------

    _type: $ => choice($.type_name, $.list_type, $.optional_type),
    optional_type: $ => seq($._type, token.immediate('?')),
    list_type: $ => seq('[', $._type, ']'),
    type_name: $ => prec.right(seq(
      $.identifier,
      repeat(seq('.', $.identifier)),
      optional(seq('<', $._type, repeat(seq(',', $._type)), '>')),
    )),

    // ---- Lexical ---------------------------------------------------------

    identifier: _ => /[A-Za-z_][A-Za-z0-9_]*/,
    token: _ => /\$[A-Za-z_][A-Za-z0-9_]*/,
    _hash: _ => /#[A-Za-z0-9_]+(-[A-Za-z0-9_]+)*/,
    _int: _ => /[0-9]+/,
    number: _ => /[0-9]+(\.[0-9]+)?(px|%|deg|ch|ms|s)?/,
    boolean: _ => choice('true', 'false'),
    null: _ => 'null',

    string: $ => seq(
      '"',
      repeat(choice($._string_content, $.escape_sequence)),
      token.immediate('"'),
    ),
    _string_content: _ => token.immediate(prec(1, /[^"\\\n]+/)),
    escape_sequence: _ => token.immediate(seq('\\', choice(/["\\ntr0]/, /u\{[0-9A-Fa-f]+\}/))),
  },
});

/** `a, b, c` with no trailing comma (a `use` declaration's clauses). */
function commaSep1NoTrail(rule) {
  return seq(rule, repeat(seq(',', rule)));
}

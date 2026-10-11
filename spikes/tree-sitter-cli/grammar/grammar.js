// A toy grammar for spike 2 (decisions.md m5-infra): enough of grammar.js's
// DSL (seq, choice, repeat, optional, prec, field, token, regex) to tell
// whether `tree-sitter generate` evaluates JavaScript without node.
module.exports = grammar({
  name: 'spike',
  extras: $ => [/\s/, $.comment],
  word: $ => $.identifier,
  rules: {
    source_file: $ => repeat($._item),
    _item: $ => choice($.binding, $.panel),
    binding: $ => seq('let', field('name', $.identifier), '=', field('value', $._expr)),
    panel: $ => seq('panel', field('name', $.identifier), '{', repeat($.binding), '}'),
    _expr: $ => choice($.identifier, $.number, $.binary),
    binary: $ => prec.left(1, seq($._expr, choice('+', '-'), $._expr)),
    identifier: _ => /[a-z_][a-z0-9_]*/,
    number: _ => token(/\d+/),
    comment: _ => token(seq('//', /.*/)),
  },
});

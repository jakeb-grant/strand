// The external scanner for `.strand` (docs/grammar.md, "Lines and
// termination"). It decides one thing: where an item ends without a `;`.
//
// The grammar asks for `_line_break` only where an item may end. There a
// line break is a separator unless the next line (past blank lines and
// comments) starts with a continuation token (rule 3):
//
//   ??  ||  &&  ==  !=  <  <=  >  >=  +  -  *  /  %
//   .  ?.  ?  :  ~  =>  <->  else
//
// with one exception: a `-` that touches what follows (`-1`, `-x`) starts
// a new item, and `- 1` continues the line. Where the grammar cannot end
// an item (inside `( … )` and `[ … ]`, after an operator or a `,`), the
// scanner is not asked and the line break is whitespace (rules 1 and 4).
// The token is the line break alone, so comments after it stay comments.
//
// Rule 2's other half: an item whose last token is a `}` may be followed
// by the next item on its line. The grammar gives that shape to items
// that end with a block; an item that ends in an expression may end in a
// `}` too (`x = match a { _ => 1 } y = 2`, a lambda's block). So the
// scanner lexes `}` where it is valid and remembers the column after it;
// asked for a separator right there, with the next token on the same line
// starting an item rather than continuing this one, it gives a zero-width
// one. Which tokens continue follows the parser:
//
// - A word that only continues an item (`else`, and `persist`, `while`,
//   `key` where no value term may follow) is not a new item.
// - Where a prop value may take another space-separated term
//   (`_group_term` valid), a word, string, `#`, `$`, digit, `(` or `[`
//   is the value's next term (`margin: match a { _ => 1 } 4`), except a
//   tree keyword, `else` and a word followed by `:` (the next prop, `key:`
//   included), which end the value as the parser's `starts_term` says.
// - A `(` or `[` continues only when it touches the `}` of a `match` (a
//   call or an index, which the grammar marks with `_postfix`); spaced, or
//   after a lambda's or a statement block's `}`, it starts an item.
// - An `@` (an attribute) always starts an item.
// - A `-` continues where a binary `-` may follow (`_infix_minus` valid);
//   elsewhere (after a statement block's `}`) it starts one (`-1 => …`).
// - Any other operator, `;` or `}` continues the item or separates it
//   already.
//
// It also lexes the `.` of a qualified element (`tray.Row`, grammar.md
// "Qualified names") where one is valid, as the compiler's parser takes
// it: touching the word before it and the name after it, and the name
// not followed by a touching `(`, `[`, `.` or `?.` or by an assignment,
// which make the line a handler statement (`n.expire()`, `a.b = 1`).

#include "tree_sitter/parser.h"

#include <stdbool.h>
#include <stdint.h>

#include <stdlib.h>
#include <string.h>

enum TokenType {
  LINE_BREAK,
  ERROR_SENTINEL,
  RBRACE,
  GROUP_TERM,
  INFIX_MINUS,
  POSTFIX,
  QUALIFIED_DOT,
};

// The column just after the last `}` the scanner lexed, plus one (0: none
// since the last line break or zero-width separator).
typedef struct {
  uint32_t brace_end;
} Scanner;

void *tree_sitter_strand_external_scanner_create(void) { return calloc(1, sizeof(Scanner)); }

void tree_sitter_strand_external_scanner_destroy(void *payload) { free(payload); }

unsigned tree_sitter_strand_external_scanner_serialize(void *payload, char *buffer) {
  Scanner *s = payload;
  memcpy(buffer, &s->brace_end, sizeof s->brace_end);
  return sizeof s->brace_end;
}

void tree_sitter_strand_external_scanner_deserialize(void *payload, const char *buffer,
                                                     unsigned length) {
  Scanner *s = payload;
  s->brace_end = 0;
  if (length == sizeof s->brace_end) memcpy(&s->brace_end, buffer, sizeof s->brace_end);
}

static bool is_ident_char(int32_t c) {
  return c == '_' || (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') ||
         (c >= '0' && c <= '9');
}

static bool is_space(int32_t c) { return c == ' ' || c == '\t' || c == '\r' || c == '\f'; }

// Whether the token at the lexer continues the line before it. The lexer
// is past the line break; it may be advanced, since the token ends at the
// line break (`mark_end`).
static bool continues(TSLexer *lexer) {
  // Past blank lines and whole-line comments to the next token.
  for (;;) {
    int32_t c = lexer->lookahead;
    if (is_space(c) || c == '\n') {
      lexer->advance(lexer, false);
      continue;
    }
    if (c == '/') {
      lexer->advance(lexer, false);
      if (lexer->lookahead != '/') return true;  // `/` divides
      while (lexer->lookahead != '\n' && !lexer->eof(lexer)) lexer->advance(lexer, false);
      continue;
    }
    break;
  }
  if (lexer->eof(lexer)) return false;
  switch (lexer->lookahead) {
    case '?':  // ?? ?. ?
    case '|':  // ||
    case '&':  // &&
    case '<':  // < <= <->
    case '>':  // > >=
    case '+':
    case '*':
    case '%':
    case '.':
    case ':':
    case '~':
      return true;
    case '=':  // == =>, not a lone `=`
      lexer->advance(lexer, false);
      return lexer->lookahead == '=' || lexer->lookahead == '>';
    case '!':  // !=, not a prefix `!`
      lexer->advance(lexer, false);
      return lexer->lookahead == '=';
    case '-':  // `- 1` continues, `-1` starts an item
      lexer->advance(lexer, false);
      return is_space(lexer->lookahead) || lexer->lookahead == '\n';
    case 'e': {
      const char *word = "else";
      for (const char *w = word; *w; w++) {
        if (lexer->lookahead != *w) return false;
        lexer->advance(lexer, false);
      }
      return !is_ident_char(lexer->lookahead);
    }
    default:
      return false;
  }
}

static bool is_ident_start(int32_t c) {
  return c == '_' || (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z');
}

// The parser's TREE_KEYWORDS (syntax/parser/mod.rs): words that start a
// tree item, and so end a prop value's space-separated terms.
static const char *const tree_keywords[] = {
    "when", "if", "match", "for", "on", "after", "every",
    "enter", "exit", "state", "let", "slot", "set", "play",
};

// Whether the next token, past spaces, line breaks and comments, is a `:`
// (the parser's `nth_kind(1) == Colon`). Advances the lexer.
static bool colon_follows(TSLexer *lexer) {
  for (;;) {
    int32_t c = lexer->lookahead;
    if (is_space(c) || c == '\n') {
      lexer->advance(lexer, false);
      continue;
    }
    if (c == '/') {
      lexer->advance(lexer, false);
      if (lexer->lookahead != '/') return false;
      while (lexer->lookahead != '\n' && !lexer->eof(lexer)) lexer->advance(lexer, false);
      continue;
    }
    return c == ':';
  }
}

// Whether the token at the lexer, on the line of a `}` that may end an
// item, starts the next item. `spaced`: whitespace separates it from the
// `}`. Advances the lexer (the separator is zero-width, so its end is
// already marked).
static bool starts_item(TSLexer *lexer, const bool *valid_symbols, bool spaced) {
  bool term = valid_symbols[GROUP_TERM];
  int32_t c = lexer->lookahead;
  if (is_ident_start(c)) {
    // Words that only continue an item.
    static const char *const continuing[] = {"persist", "while", "key"};
    char word[9];
    unsigned n = 0;
    while (is_ident_char(lexer->lookahead)) {
      if (n < sizeof word - 1) word[n] = (char)lexer->lookahead;
      n++;
      lexer->advance(lexer, false);
    }
    if (n < sizeof word) {
      word[n] = 0;
      if (strcmp(word, "else") == 0) return false;
      for (unsigned i = 0; i < sizeof tree_keywords / sizeof *tree_keywords; i++) {
        if (strcmp(word, tree_keywords[i]) == 0) return true;
      }
      // Where a value may take another term, `key`, `persist` and `while`
      // are names like any other (the parser's `starts_term`): a term, or
      // with a `:` after them the next prop.
      if (!term) {
        for (unsigned i = 0; i < sizeof continuing / sizeof *continuing; i++) {
          if (strcmp(word, continuing[i]) == 0) return false;
        }
      }
    }
    return !term || colon_follows(lexer);
  }
  switch (c) {
    case '@':  // an attribute, never a term
      return true;
    case '#':  // a selector
    case '$':  // a token entry
    case '"':
      return !term;
    case '(':  // a call or an index touches; a group term may be spaced
    case '[':
      return !term && (spaced || !valid_symbols[POSTFIX]);
    case '-':  // a binary `-`, or a sign starting the next item
      return !valid_symbols[INFIX_MINUS];
    case '!':  // a prefix `!`, not `!=`
      lexer->advance(lexer, false);
      return lexer->lookahead != '=' && !term;
    default:
      // Digits (a keyframe offset) too: every other token continues the
      // item or is a separator of its own.
      return c >= '0' && c <= '9' && !term;
  }
}

bool tree_sitter_strand_external_scanner_scan(void *payload, TSLexer *lexer,
                                              const bool *valid_symbols) {
  Scanner *s = payload;
  if (valid_symbols[ERROR_SENTINEL]) return false;
  // Right after a `}` this scanner lexed: no token since (one would have
  // moved the column, or been lexed here and cleared it).
  bool after_brace =
      s->brace_end != 0 && valid_symbols[LINE_BREAK] && lexer->get_column(lexer) + 1 == s->brace_end;
  bool spaced = is_space(lexer->lookahead);
  while (is_space(lexer->lookahead)) lexer->advance(lexer, true);
  if (valid_symbols[LINE_BREAK] && lexer->lookahead == '\n') {
    lexer->advance(lexer, false);
    lexer->mark_end(lexer);
    lexer->result_symbol = LINE_BREAK;
    s->brace_end = 0;
    return !continues(lexer);
  }
  if (valid_symbols[QUALIFIED_DOT] && !spaced && lexer->lookahead == '.') {
    lexer->advance(lexer, false);
    lexer->mark_end(lexer);
    if (!is_ident_start(lexer->lookahead)) return false;
    while (is_ident_char(lexer->lookahead)) lexer->advance(lexer, false);
    int32_t c = lexer->lookahead;
    if (c == '(' || c == '[' || c == '.') return false;
    if (c == '?') {
      lexer->advance(lexer, false);
      if (lexer->lookahead == '.') return false;
    } else {
      while (is_space(lexer->lookahead) || lexer->lookahead == '\n') lexer->advance(lexer, false);
      c = lexer->lookahead;
      if (c == '=' || c == '+' || c == '-' || c == '*' || c == '/') {
        lexer->advance(lexer, false);
        int32_t d = lexer->lookahead;
        if (c == '=' ? d != '=' && d != '>' : d == '=') return false;
      }
    }
    lexer->result_symbol = QUALIFIED_DOT;
    s->brace_end = 0;
    return true;
  }
  if (valid_symbols[RBRACE]) {

    while (is_space(lexer->lookahead) || lexer->lookahead == '\n') lexer->advance(lexer, true);
    if (lexer->lookahead == '}') {
      lexer->advance(lexer, false);
      lexer->mark_end(lexer);
      lexer->result_symbol = RBRACE;
      s->brace_end = lexer->get_column(lexer) + 1;
      return true;
    }
    // No line break was skipped if one is valid: it was handled above.
  }
  if (after_brace) {
    lexer->mark_end(lexer);
    lexer->result_symbol = LINE_BREAK;
    s->brace_end = 0;
    return starts_item(lexer, valid_symbols, spaced);
  }
  return false;
}

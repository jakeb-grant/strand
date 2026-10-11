// The external scanner for `.strand` (docs/grammar.md, "Lines and
// termination"). It decides one thing: whether a line break ends an item.
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

#include "tree_sitter/parser.h"

#include <stdbool.h>
#include <stdint.h>

enum TokenType {
  LINE_BREAK,
  ERROR_SENTINEL,
};

void *tree_sitter_strand_external_scanner_create(void) { return NULL; }

void tree_sitter_strand_external_scanner_destroy(void *payload) { (void)payload; }

unsigned tree_sitter_strand_external_scanner_serialize(void *payload, char *buffer) {
  (void)payload;
  (void)buffer;
  return 0;
}

void tree_sitter_strand_external_scanner_deserialize(void *payload, const char *buffer,
                                                     unsigned length) {
  (void)payload;
  (void)buffer;
  (void)length;
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

bool tree_sitter_strand_external_scanner_scan(void *payload, TSLexer *lexer,
                                              const bool *valid_symbols) {
  (void)payload;
  if (valid_symbols[ERROR_SENTINEL] || !valid_symbols[LINE_BREAK]) return false;
  while (is_space(lexer->lookahead)) lexer->advance(lexer, true);
  if (lexer->lookahead != '\n') return false;
  lexer->advance(lexer, false);
  lexer->mark_end(lexer);
  lexer->result_symbol = LINE_BREAK;
  return !continues(lexer);
}

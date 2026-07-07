// External scanner for keron raw multiline strings:
// `r"""..."""`, `r#"""..."""#`, `r##"""..."""##`, ... with the same
// number of `#`s on both sides. The hash count is unbounded, which a
// context-free grammar cannot backreference — hence this scanner.
//
// Stateless: serialize/deserialize carry nothing, because a raw
// string is consumed as a single token in one scan call.

#include "tree_sitter/parser.h"

#include <stdbool.h>
#include <wctype.h>

enum TokenType {
  RAW_STRING,
};

void *tree_sitter_keron_external_scanner_create(void) { return NULL; }

void tree_sitter_keron_external_scanner_destroy(void *payload) {
  (void)payload;
}

unsigned tree_sitter_keron_external_scanner_serialize(void *payload,
                                                      char *buffer) {
  (void)payload;
  (void)buffer;
  return 0;
}

void tree_sitter_keron_external_scanner_deserialize(void *payload,
                                                    const char *buffer,
                                                    unsigned length) {
  (void)payload;
  (void)buffer;
  (void)length;
}

static void advance(TSLexer *lexer) { lexer->advance(lexer, false); }

bool tree_sitter_keron_external_scanner_scan(void *payload, TSLexer *lexer,
                                             const bool *valid_symbols) {
  (void)payload;
  if (!valid_symbols[RAW_STRING]) {
    return false;
  }

  // External scanners run before extras are skipped; consume leading
  // whitespace as skipped content or the internal lexer never yields
  // the `r` position to us.
  while (iswspace((wint_t)lexer->lookahead)) {
    lexer->advance(lexer, true);
  }

  if (lexer->lookahead != 'r') {
    return false;
  }
  advance(lexer);

  unsigned hashes = 0;
  while (lexer->lookahead == '#') {
    hashes++;
    advance(lexer);
  }

  // A bare `r` identifier or `r#...` comment-after-identifier is not a
  // raw string; returning false resets the lexer to where we started.
  for (int i = 0; i < 3; i++) {
    if (lexer->lookahead != '"') {
      return false;
    }
    advance(lexer);
  }

  // Body: consume until a run of >= 3 quotes followed by exactly
  // `hashes` hash characters. A shorter quote run or hash run is body
  // content and scanning continues.
  while (true) {
    if (lexer->eof(lexer)) {
      return false;
    }
    if (lexer->lookahead != '"') {
      advance(lexer);
      continue;
    }
    unsigned quotes = 0;
    while (lexer->lookahead == '"') {
      quotes++;
      advance(lexer);
    }
    if (quotes < 3) {
      continue;
    }
    unsigned trailing = 0;
    while (trailing < hashes && lexer->lookahead == '#') {
      trailing++;
      advance(lexer);
    }
    if (trailing == hashes) {
      lexer->mark_end(lexer);
      lexer->result_symbol = RAW_STRING;
      return true;
    }
  }
}

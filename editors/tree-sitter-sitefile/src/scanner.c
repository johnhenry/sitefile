// External scanner for the Sitefile grammar: the line-oriented, context
// sensitive parts of src/language/scanner.ts.
//
// - _newline ends an instruction (zero width at end of file).
// - line_continuation is `\` at the end of a line plus any blank lines after
//   it; a comment line inside a continuation is a comment followed by a
//   hidden _continuation_newline (which also absorbs blank lines).
// - comment: a line whose first non-blank character is `#`.
// - heredocs: `<<NAME` / `<<-NAME` (an identifier followed by whitespace or the
//   end of the line). After the header line ends, every line up to one that is
//   exactly NAME (after stripping leading tabs for `<<-`) is body. RENDER
//   bodies are split around `${` (template tags are parsed by grammar.js) and
//   `$${` (template_escape); COPY and TRANSFORM bodies are one raw token.
// - _resource_name: the `name` of `name:` / `name:/path` in source position.

#include "tree_sitter/alloc.h"
#include "tree_sitter/parser.h"

#include <stdbool.h>
#include <stdint.h>
#include <string.h>

enum TokenType {
  NEWLINE,
  LINE_CONTINUATION,
  CONTINUATION_NEWLINE,
  COMMENT,
  DIRECTIVE,
  HEREDOC_MARKER,
  HEREDOC_MARKER_STRIP,
  HEREDOC_DELIMITER,
  HEREDOC_RAW,
  TEMPLATE_TEXT,
  TEMPLATE_ESCAPE,
  HEREDOC_END,
  RESOURCE_NAME,
  BLANK_LINES,
  ERROR_SENTINEL,
};

enum Mode {
  MODE_NONE,      // ordinary lines
  MODE_DELIMITER, // after `<<` / `<<-`: the delimiter name comes next
  MODE_HEADER,    // rest of the instruction line that started a heredoc
  MODE_BODY,      // heredoc body lines, up to the closing line
};

#define MAX_DELIMITER 255

typedef struct {
  uint8_t mode;
  bool strip;           // `<<-`: leading tabs are stripped from the closing line
  bool continuing;      // the last token was a continuation (we are inside one)
  bool comment_pending; // a comment inside a continuation still owes its newline
  uint8_t delimiter_length;
  char delimiter[MAX_DELIMITER];
} Scanner;

static inline void advance(TSLexer *lexer) { lexer->advance(lexer, false); }
static inline void skip(TSLexer *lexer) { lexer->advance(lexer, true); }

static inline bool is_ident_start(int32_t c) { return (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') || c == '_'; }
static inline bool is_ident_char(int32_t c) { return is_ident_start(c) || (c >= '0' && c <= '9'); }
static inline bool is_blank(int32_t c) { return c == ' ' || c == '\t' || c == '\f' || c == '\v'; }
static inline bool at_line_end(TSLexer *lexer) {
  return lexer->eof(lexer) || lexer->lookahead == '\n' || lexer->lookahead == '\r';
}

/** Consume `\r?\n` (or a lone `\r`). */
static void consume_newline(TSLexer *lexer) {
  if (lexer->lookahead == '\r') advance(lexer);
  if (lexer->lookahead == '\n') advance(lexer);
}

/** Mark the token end here, then extend it over any whitespace-only lines. */
static void absorb_blank_lines(TSLexer *lexer) {
  lexer->mark_end(lexer);
  for (;;) {
    while (is_blank(lexer->lookahead) || lexer->lookahead == '\r') {
      if (lexer->lookahead == '\r') {
        advance(lexer);
        if (lexer->lookahead != '\n') return;
        break;
      }
      advance(lexer);
    }
    if (lexer->lookahead != '\n') return;
    advance(lexer);
    lexer->mark_end(lexer);
  }
}

/**
 * At the start of a body line: is it the closing line? Advances over the
 * characters it compares (tabs for `<<-`, then the delimiter); `consumed`
 * reports whether it advanced at all.
 */
static bool match_terminator(Scanner *s, TSLexer *lexer, bool *consumed) {
  *consumed = false;
  if (s->strip) {
    while (lexer->lookahead == '\t') {
      advance(lexer);
      *consumed = true;
    }
  }
  for (unsigned i = 0; i < s->delimiter_length; i++) {
    if (lexer->eof(lexer) || lexer->lookahead != (int32_t)(unsigned char)s->delimiter[i]) return false;
    advance(lexer);
    *consumed = true;
  }
  return at_line_end(lexer);
}

static void end_heredoc(Scanner *s) {
  s->mode = MODE_NONE;
  s->strip = false;
  s->delimiter_length = 0;
}

static bool scan_body(Scanner *s, TSLexer *lexer, const bool *valid, bool template) {
  enum TokenType text = template ? TEMPLATE_TEXT : HEREDOC_RAW;
  bool has_text = false;

  if (lexer->get_column(lexer) == 0) {
    bool consumed = false;
    if (match_terminator(s, lexer, &consumed)) {
      if (!valid[HEREDOC_END] && !valid[ERROR_SENTINEL]) return false;
      lexer->mark_end(lexer);
      lexer->result_symbol = HEREDOC_END;
      end_heredoc(s);
      return true;
    }
    has_text = consumed;
  }

  for (;;) {
    if (lexer->eof(lexer)) {
      // Unterminated body: everything up to the end of the file.
      lexer->mark_end(lexer);
      if (!has_text) return false;
      lexer->result_symbol = text;
      return true;
    }
    int32_t c = lexer->lookahead;
    if (c == '\n') {
      advance(lexer);
      has_text = true;
      lexer->mark_end(lexer);
      bool consumed = false;
      if (match_terminator(s, lexer, &consumed)) {
        lexer->result_symbol = text; // ends at the start of the closing line
        return true;
      }
      continue;
    }
    if (template && c == '$') {
      lexer->mark_end(lexer);
      advance(lexer);
      if (lexer->lookahead == '{') {
        // `${`: a template tag (lexed by the grammar).
        if (!has_text) return false;
        lexer->result_symbol = text;
        return true;
      }
      if (lexer->lookahead == '$') {
        advance(lexer);
        if (lexer->lookahead == '{') {
          // `$${` writes a literal `${`.
          if (has_text) {
            lexer->result_symbol = text;
            return true;
          }
          advance(lexer);
          lexer->mark_end(lexer);
          lexer->result_symbol = TEMPLATE_ESCAPE;
          return true;
        }
      }
      has_text = true;
      continue;
    }
    advance(lexer);
    has_text = true;
  }
}

static void after_line_token(Scanner *s) {
  s->continuing = false;
  s->comment_pending = false;
}

bool tree_sitter_sitefile_external_scanner_scan(void *payload, TSLexer *lexer, const bool *valid) {
  Scanner *s = (Scanner *)payload;
  bool recovery = valid[ERROR_SENTINEL];

  if (s->mode == MODE_BODY) {
    if (valid[TEMPLATE_TEXT] && !recovery) return scan_body(s, lexer, valid, true);
    if (valid[HEREDOC_RAW] || valid[HEREDOC_END] || recovery) return scan_body(s, lexer, valid, false);
    return false;
  }

  if (s->mode == MODE_DELIMITER) {
    if (!valid[HEREDOC_DELIMITER] && !recovery) return false;
    for (unsigned i = 0; i < s->delimiter_length; i++) advance(lexer);
    lexer->mark_end(lexer);
    lexer->result_symbol = HEREDOC_DELIMITER;
    s->mode = MODE_HEADER;
    return true;
  }

  bool line_start = lexer->get_column(lexer) == 0;

  // The newline after a comment line inside a continuation does not end the
  // instruction.
  if (s->comment_pending) {
    while (is_blank(lexer->lookahead)) skip(lexer);
    if (lexer->lookahead == '\n' || lexer->lookahead == '\r') {
      consume_newline(lexer);
      absorb_blank_lines(lexer);
      lexer->result_symbol = CONTINUATION_NEWLINE;
      s->comment_pending = false;
      s->continuing = true;
      return true;
    }
  }

  while (is_blank(lexer->lookahead)) skip(lexer);

  if (line_start && lexer->lookahead == '#' && valid[COMMENT]) {
    // `#[ \t]*sitefile[ \t]*=` (ASCII case-insensitive): the version directive.
    static const char NAME[] = "sitefile";
    advance(lexer);
    while (lexer->lookahead == ' ' || lexer->lookahead == '\t') advance(lexer);
    unsigned matched = 0;
    while (matched < 8 && !at_line_end(lexer)) {
      int32_t c = lexer->lookahead;
      if (c >= 'A' && c <= 'Z') c += 'a' - 'A';
      if (c != NAME[matched]) break;
      matched++;
      advance(lexer);
    }
    bool directive = false;
    if (matched == 8) {
      while (lexer->lookahead == ' ' || lexer->lookahead == '\t') advance(lexer);
      directive = lexer->lookahead == '=';
    }
    while (!at_line_end(lexer)) advance(lexer);
    lexer->mark_end(lexer);
    lexer->result_symbol = directive && valid[DIRECTIVE] ? DIRECTIVE : COMMENT;
    if (s->continuing) s->comment_pending = true;
    return true;
  }

  if (lexer->lookahead == '\\') {
    advance(lexer);
    if (lexer->lookahead == '\r') advance(lexer);
    if (lexer->lookahead == '\n') {
      advance(lexer);
      absorb_blank_lines(lexer);
    } else if (lexer->eof(lexer)) {
      lexer->mark_end(lexer);
    } else {
      return false; // an escape such as `\ ` or `\\`
    }
    lexer->result_symbol = LINE_CONTINUATION;
    s->continuing = true;
    s->comment_pending = false;
    return true;
  }

  if (lexer->lookahead == '\n' || lexer->lookahead == '\r') {
    // Between instructions: blank lines. Inside an instruction, a newline
    // where none is expected is left to the parser as an error, so a missing
    // operand never swallows the next line.
    if (!valid[NEWLINE] && valid[BLANK_LINES]) {
      consume_newline(lexer);
      absorb_blank_lines(lexer);
      lexer->result_symbol = BLANK_LINES;
      return true;
    }
    if (!valid[NEWLINE]) return false;
    consume_newline(lexer);
    lexer->mark_end(lexer);
    lexer->result_symbol = NEWLINE;
    after_line_token(s);
    if (s->mode == MODE_HEADER) s->mode = MODE_BODY;
    return true;
  }

  if (lexer->eof(lexer)) {
    if (!valid[NEWLINE] || recovery) return false;
    lexer->mark_end(lexer);
    lexer->result_symbol = NEWLINE;
    after_line_token(s);
    if (s->mode == MODE_HEADER) s->mode = MODE_BODY;
    return true;
  }

  if (recovery) return false;

  if (lexer->lookahead == '<' && (valid[HEREDOC_MARKER] || valid[HEREDOC_MARKER_STRIP])) {
    advance(lexer);
    if (lexer->lookahead != '<') return false;
    advance(lexer);
    bool strip = false;
    if (lexer->lookahead == '-') {
      advance(lexer);
      strip = true;
    }
    lexer->mark_end(lexer);
    if (!is_ident_start(lexer->lookahead)) return false;
    uint8_t length = 0;
    char delimiter[MAX_DELIMITER];
    while (is_ident_char(lexer->lookahead)) {
      if (length == MAX_DELIMITER) return false;
      delimiter[length++] = (char)lexer->lookahead;
      advance(lexer);
    }
    if (!(is_blank(lexer->lookahead) || at_line_end(lexer) || lexer->lookahead == '\\')) return false;
    s->mode = MODE_DELIMITER;
    s->strip = strip;
    s->delimiter_length = length;
    memcpy(s->delimiter, delimiter, length);
    after_line_token(s);
    lexer->result_symbol = strip ? HEREDOC_MARKER_STRIP : HEREDOC_MARKER;
    return true;
  }

  if (valid[RESOURCE_NAME] && is_ident_start(lexer->lookahead)) {
    while (is_ident_char(lexer->lookahead)) advance(lexer);
    if (lexer->lookahead != ':') return false;
    lexer->mark_end(lexer);
    advance(lexer);
    if (lexer->lookahead == '/' || is_blank(lexer->lookahead) || at_line_end(lexer)) {
      lexer->result_symbol = RESOURCE_NAME;
      after_line_token(s);
      return true;
    }
    return false;
  }

  return false;
}

void *tree_sitter_sitefile_external_scanner_create(void) {
  Scanner *s = (Scanner *)ts_calloc(1, sizeof(Scanner));
  return s;
}

void tree_sitter_sitefile_external_scanner_destroy(void *payload) { ts_free(payload); }

unsigned tree_sitter_sitefile_external_scanner_serialize(void *payload, char *buffer) {
  Scanner *s = (Scanner *)payload;
  buffer[0] = (char)s->mode;
  buffer[1] = (char)s->strip;
  buffer[2] = (char)s->continuing;
  buffer[3] = (char)s->comment_pending;
  buffer[4] = (char)s->delimiter_length;
  memcpy(&buffer[5], s->delimiter, s->delimiter_length);
  return 5 + s->delimiter_length;
}

void tree_sitter_sitefile_external_scanner_deserialize(void *payload, const char *buffer, unsigned length) {
  Scanner *s = (Scanner *)payload;
  memset(s, 0, sizeof(Scanner));
  if (length < 5) return;
  s->mode = (uint8_t)buffer[0];
  s->strip = buffer[1] != 0;
  s->continuing = buffer[2] != 0;
  s->comment_pending = buffer[3] != 0;
  s->delimiter_length = (uint8_t)buffer[4];
  if (5u + s->delimiter_length > length) s->delimiter_length = (uint8_t)(length - 5);
  memcpy(s->delimiter, &buffer[5], s->delimiter_length);
}

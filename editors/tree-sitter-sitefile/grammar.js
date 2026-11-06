/**
 * @file Tree-sitter grammar for Sitefile (docs/LANGUAGE.md).
 * @license MIT
 *
 * Mirrors src/language/scanner.ts and parser.ts (and the template syntax in
 * src/render.ts). The rules that need context live in src/scanner.c:
 * - one instruction per line: `_newline` ends it (also at end of file);
 * - a line ending in `\` continues; blank lines inside a continuation are
 *   skipped (`line_continuation`);
 * - a comment is a line whose first non-blank character is `#`, also inside
 *   a continuation; `#` elsewhere is ordinary text;
 * - heredocs `<<NAME` / `<<-NAME` on COPY, RENDER and TRANSFORM: the body ends
 *   at a line that is exactly NAME (`<<-` also strips leading tabs);
 * - `name:` / `name:/path` import references in source position.
 *
 * Instruction and clause keywords are case-insensitive regexes. Value words
 * (types, import kinds, export formats, …) are parsed as plain words and
 * classified by queries/highlights.scm, like the parser does after reading
 * them.
 *
 * RENDER heredoc bodies are templates: `${x}`, `${#each}`, `${#if}`, … are
 * parsed in place as children of `heredoc_body`. Injections (by delimiter:
 * HTML, CSS, JS, …) exclude those children, so the embedded language sees the
 * surrounding text only and template tags keep their own highlighting.
 */

/// <reference types="tree-sitter-cli/dsl" />
// @ts-check

import { KEYWORDS, caseInsensitive } from './keywords.js';

const IDENT = /[A-Za-z_][A-Za-z0-9_]*/;
const KEY = /[A-Za-z0-9_-]+/;

/** All keyword tokens, declared first so they win ties with bare words (rule order). */
const KEYWORD_WORDS = [...new Set([...KEYWORDS.instructions, ...KEYWORDS.clauses, ...KEYWORDS.unsupported])];
const kwRule = (word) => `_kw_${word.toLowerCase()}`;
const keywordRules = Object.fromEntries(
  KEYWORD_WORDS.map((word) => [kwRule(word), () => token(new RegExp(caseInsensitive(word)))]),
);

/** A case-insensitive keyword, visible in the tree as the anonymous node "WORD". */
const kw = ($, word) => alias($[kwRule(word)], word);

export default grammar({
  name: 'sitefile',

  externals: ($) => [
    $._newline,
    $.line_continuation,
    $._continuation_newline,
    $.comment,
    $._heredoc_marker,
    $._heredoc_marker_strip,
    $.heredoc_delimiter,
    $._heredoc_raw,
    $._template_text,
    $.template_escape,
    $.heredoc_end,
    $._resource_name,
    $._blank_lines,
    $._error_sentinel,
  ],

  extras: ($) => [/[ \t\f\v]/, $.line_continuation, $._continuation_newline, $.comment],

  supertypes: ($) => [$._instruction],

  rules: {
    // Newlines are not whitespace: one ends an instruction (`_newline`), and
    // between instructions they are `_blank_lines`.
    source_file: ($) => repeat(choice(seq($._instruction, $._newline), $._blank_lines)),

    // Keyword tokens come first: on a tie with a bare word, the earlier rule wins.
    ...keywordRules,
    scratch: () => token(new RegExp(caseInsensitive(KEYWORDS.scratch))),
    integrity: () => token(/sha256:[0-9a-f]{64}/),
    number: () => token(/-?[0-9]+(\.[0-9]+)?/),
    boolean: () => token(new RegExp(`${caseInsensitive('true')}|${caseInsensitive('false')}`)),
    placeholder: () => token(/\{(input|output|option\.[A-Za-z0-9_-]+)\}/),
    _path_start: () => token(/(\.\.?)?\/[^\s$\\]*/),
    _url_start: () => token(/[Hh][Tt][Tt][Pp][Ss]?:\/\/[^\s$\\]*/),
    _word_start: () => token(/[^\s"'$\\-][^\s$\\]*/),
    _dash_word: () => token(/-[^\s$\\-][^\s$\\]*/),
    _value_word: () => token(/[^\s\\]+/),
    identifier: () => IDENT,
    key: () => KEY,

    _instruction: ($) =>
      choice(
        $.from_instruction,
        $.stage_instruction,
        $.arg_instruction,
        $.let_instruction,
        $.import_instruction,
        $.copy_instruction,
        $.render_instruction,
        $.transform_instruction,
        $.plugin_instruction,
        $.tool_instruction,
        $.sitemap_instruction,
        $.feed_instruction,
        $.redirect_instruction,
        $.header_instruction,
        $.fingerprint_instruction,
        $.checklinks_instruction,
        $.export_instruction,
        $.unsupported_instruction,
      ),

    // FROM scratch|<base>|image <ref> [AT <dir>] [AS name]
    from_instruction: ($) =>
      seq(kw($, 'FROM'), field('base', $._base), optional(seq(kw($, 'AS'), field('alias', $.identifier)))),

    // STAGE name [FROM scratch|<base>|image <ref> [AT <dir>]]
    stage_instruction: ($) =>
      seq(kw($, 'STAGE'), field('name', $.identifier), optional(seq(kw($, 'FROM'), field('base', $._base)))),

    _base: ($) => choice($.scratch, $.image_base, $._operand),

    // image <ref> [AT <dir>]: an OCI site image. The parser reads `image` as a stage or directory name
    // when nothing (or AS) follows it; here that is an image_base without a reference.
    image_base: ($) =>
      seq(kw($, 'IMAGE'), optional(seq(field('reference', $._operand), optional(seq(kw($, 'AT'), field('at', $._operand)))))),

    // ARG NAME[: type] [= value]
    arg_instruction: ($) =>
      seq(kw($, 'ARG'), field('name', $.identifier), optional($._type_annotation), optional(seq('=', field('value', $._value)))),

    // LET NAME[: type] = value
    let_instruction: ($) =>
      seq(kw($, 'LET'), field('name', $.identifier), optional($._type_annotation), '=', field('value', $._value)),

    _type_annotation: ($) => seq(':', field('type', alias($.identifier, $.type))),

    // IMPORT name FROM <uri> [AS kind] [INTEGRITY sha256:…] [SORT BY key [ASC|DESC]]
    import_instruction: ($) =>
      seq(
        kw($, 'IMPORT'),
        field('name', $.identifier),
        kw($, 'FROM'),
        field('source', $._operand),
        repeat(
          choice(
            seq(kw($, 'AS'), field('kind', alias($.identifier, $.import_kind))),
            $._integrity_clause,
            field('sort', $.sort_clause),
          ),
        ),
      ),

    sort_clause: ($) =>
      seq(kw($, 'SORT'), kw($, 'BY'), field('key', $.identifier), optional(field('order', choice(kw($, 'ASC'), kw($, 'DESC'))))),

    _integrity_clause: ($) => seq(kw($, 'INTEGRITY'), field('integrity', choice($.integrity, $._operand))),

    // COPY [--overwrite] [--from=<stage>] <source>|<<NAME <dest>
    copy_instruction: ($) =>
      seq(
        kw($, 'COPY'),
        repeat(field('flag', $.flag)),
        choice(
          seq(field('source', $.heredoc_start), field('target', $._operand), $._raw_heredoc),
          seq(field('source', $._source), field('target', $._operand)),
        ),
      ),

    // RENDER [flags] <template>|<<NAME TO <dest> [FOR item IN list]
    render_instruction: ($) =>
      seq(
        kw($, 'RENDER'),
        repeat(field('flag', $.flag)),
        choice(
          seq(field('source', $.heredoc_start), $._render_clauses, $._template_heredoc),
          seq(field('source', $._source), $._render_clauses),
        ),
      ),

    _render_clauses: ($) => seq(kw($, 'TO'), field('target', $._operand), optional(field('loop', $.for_clause))),

    // TRANSFORM [flags] [<source>|<<NAME] WITH name TO <dest> [OPTIONS k=v …] [FOR item IN list] [ARGS …]
    transform_instruction: ($) =>
      seq(
        kw($, 'TRANSFORM'),
        repeat(field('flag', $.flag)),
        choice(
          seq(field('source', $.heredoc_start), $._transform_clauses, $._raw_heredoc),
          seq(optional(field('source', $._source)), $._transform_clauses),
        ),
      ),

    _transform_clauses: ($) =>
      seq(
        kw($, 'WITH'),
        field('plugin', $.identifier),
        kw($, 'TO'),
        field('target', $._operand),
        repeat(choice(field('options', $.options_clause), field('loop', $.for_clause))),
        optional(field('args', $.args_clause)),
      ),

    options_clause: ($) => seq(kw($, 'OPTIONS'), repeat($.option)),

    option: ($) => seq(field('key', $.identifier), '=', field('value', $._value)),

    // ARGS takes the rest of the line: every word is one argv entry.
    args_clause: ($) => seq(kw($, 'ARGS'), repeat(field('argument', choice($.placeholder, $._operand)))),

    for_clause: ($) => seq(kw($, 'FOR'), field('item', $.identifier), kw($, 'IN'), field('list', $.identifier)),

    // PLUGIN name FROM <path> [INTEGRITY sha256:…]
    plugin_instruction: ($) =>
      seq(kw($, 'PLUGIN'), field('name', $.identifier), kw($, 'FROM'), field('source', $._operand), optional($._integrity_clause)),

    // TOOL name
    tool_instruction: ($) => seq(kw($, 'TOOL'), field('name', $.identifier)),

    // SITEMAP [--overwrite] BASE <url> [TO <dest>]
    sitemap_instruction: ($) =>
      seq(
        kw($, 'SITEMAP'),
        repeat(field('flag', $.flag)),
        kw($, 'BASE'),
        field('base', $._operand),
        optional(seq(kw($, 'TO'), field('target', $._operand))),
      ),

    // FEED [--overwrite] <import> BASE <url> LINK <path> [TO <dest>] [TITLE <text>]
    feed_instruction: ($) =>
      seq(
        kw($, 'FEED'),
        repeat(field('flag', $.flag)),
        field('list', $.identifier),
        kw($, 'BASE'),
        field('base', $._operand),
        kw($, 'LINK'),
        field('link', $._operand),
        optional(seq(kw($, 'TO'), field('target', $._operand))),
        optional(seq(kw($, 'TITLE'), field('title', $._operand))),
      ),

    // REDIRECT [--overwrite] <from> <to> [STATUS 301|302|303|307|308]
    redirect_instruction: ($) =>
      seq(
        kw($, 'REDIRECT'),
        repeat(field('flag', $.flag)),
        field('from', $._operand),
        field('to', $._operand),
        optional(seq(kw($, 'STATUS'), field('status', alias($._value_word, $.status)))),
      ),

    // HEADER [--overwrite] <path-pattern> <Name>: <value …> (the name is the word ending in `:`)
    header_instruction: ($) =>
      seq(
        kw($, 'HEADER'),
        repeat(field('flag', $.flag)),
        field('pattern', $._operand),
        field('name', $._operand),
        repeat1(field('value', $._operand)),
      ),

    // FINGERPRINT [--overwrite] <pattern>… [MANIFEST <dest>]
    fingerprint_instruction: ($) =>
      seq(
        kw($, 'FINGERPRINT'),
        repeat(field('flag', $.flag)),
        repeat1(field('pattern', $._operand)),
        optional(seq(kw($, 'MANIFEST'), field('manifest', $._operand))),
      ),

    // CHECKLINKS [--warn] [--external]
    checklinks_instruction: ($) => seq(kw($, 'CHECKLINKS'), repeat(field('flag', $.flag))),

    // EXPORT <path> [AS format] [BASE <url>] [AT <root>] [COMPRESSION gzip|none]
    export_instruction: ($) =>
      seq(
        kw($, 'EXPORT'),
        field('path', $._operand),
        optional(seq(kw($, 'AS'), field('format', alias($._value_word, $.export_format)))),
        optional(seq(kw($, 'BASE'), field('base', $._operand))),
        optional(seq(kw($, 'AT'), field('root', $._operand))),
        optional(seq(kw($, 'COMPRESSION'), field('compression', alias($._value_word, $.compression)))),
      ),

    // RUN, REMOVE, … are rejected by the parser (E_UNSUPPORTED).
    unsupported_instruction: ($) =>
      seq(field('keyword', choice(...KEYWORDS.unsupported.map((w) => kw($, w)))), repeat(choice($.flag, $._operand))),

    // --overwrite, --from=<stage>, --escape=html
    flag: ($) =>
      seq(
        '--',
        field('name', alias(token.immediate(/[A-Za-z][A-Za-z0-9-]*/), $.flag_name)),
        // prec(1): `=html` must not lex as the next bare word.
        optional(seq(token.immediate(prec(1, '=')), optional(field('value', alias(token.immediate(prec(1, /[^\s\\]+/)), $.flag_value))))),
      ),

    // ---- heredocs ----------------------------------------------------------

    heredoc_start: ($) =>
      seq(choice(alias($._heredoc_marker, '<<'), alias($._heredoc_marker_strip, '<<-')), field('delimiter', $.heredoc_delimiter)),

    // COPY and TRANSFORM bodies are written byte for byte.
    _raw_heredoc: ($) =>
      seq($._newline, optional(field('body', alias($._heredoc_raw, $.heredoc_body))), field('end', $.heredoc_end)),

    // RENDER bodies are templates.
    _template_heredoc: ($) => seq($._newline, optional(field('body', $.heredoc_body)), field('end', $.heredoc_end)),

    heredoc_body: ($) =>
      repeat1(
        choice(
          $._template_text,
          $.template_escape,
          $.template_substitution,
          $.template_each,
          $.template_if,
          $.template_else,
          $.template_end,
        ),
      ),

    // ${name}, ${name.key.0}, ${name|raw}
    template_substitution: ($) =>
      seq(
        '${',
        field('name', $.identifier),
        repeat(seq('.', field('key', $.key))),
        optional(seq('|', field('filter', $.identifier))),
        '}',
      ),

    // ${#each list as item}
    template_each: ($) => seq('${', '#each', field('list', $.reference), 'as', field('item', $.identifier), '}'),

    // ${#if value} / ${#if !value}
    template_if: ($) => seq('${', '#if', optional(field('negate', '!')), field('condition', $.reference), '}'),

    template_else: () => seq('${', 'else', '}'),

    template_end: () => seq('${', choice('/each', '/if'), '}'),

    reference: ($) => seq(field('name', $.identifier), repeat(seq('.', field('key', $.key)))),

    // ---- operands ----------------------------------------------------------

    _source: ($) => choice($.resource_ref, $._operand),

    _operand: ($) => choice($.string, $.raw_string, $.path, $.url, $.word),

    _value: ($) => choice($.number, $.boolean, $._operand),

    // `name:` / `name:/inner/path`: a file or directory from an IMPORT.
    resource_ref: ($) =>
      seq(
        field('import', alias($._resource_name, $.identifier)),
        token.immediate(':'),
        optional(field('path', alias(seq(token.immediate(/\/[^\s$\\]*/), repeat($._word_part)), $.path))),
      ),

    path: ($) => seq($._path_start, repeat($._word_part)),

    url: ($) => seq($._url_start, repeat($._word_part)),

    word: ($) =>
      seq(choice($._word_start, $._dash_word, '$', $.escape_sequence, $.substitution), repeat($._word_part)),

    /** The rest of a bare word: no whitespace in between. */
    _word_part: ($) =>
      choice(
        token.immediate(/[^\s$\\]+/),
        token.immediate('$'),
        alias(token.immediate(/\\[^\r\n]/), $.escape_sequence),
        alias($._immediate_substitution, $.substitution),
      ),

    escape_sequence: () => token(/\\[^\r\n]/),

    // "double-quoted": `\` escapes and ${NAME} interpolation
    string: ($) =>
      seq(
        '"',
        repeat(
          choice(
            alias(token.immediate(prec(1, /[^"\\$\r\n]+/)), $.string_content),
            alias(token.immediate('$'), $.string_content),
            alias(token.immediate(/\\[^\r\n]/), $.escape_sequence),
            alias($._immediate_substitution, $.substitution),
          ),
        ),
        token.immediate('"'),
      ),

    // 'single-quoted': literal, no escapes or interpolation
    raw_string: () => token(/'[^'\r\n]*'/),

    // ${NAME} / ${name.key} in an operand
    substitution: ($) => seq('${', $._substitution_body),

    _immediate_substitution: ($) => seq(token.immediate('${'), $._substitution_body),

    _substitution_body: ($) =>
      seq(
        field('name', alias(token.immediate(IDENT), $.identifier)),
        repeat(seq(token.immediate('.'), field('key', alias(token.immediate(KEY), $.key)))),
        token.immediate('}'),
      ),
  },
});

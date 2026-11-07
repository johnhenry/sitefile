// Source of truth for syntaxes/*.json. Run `npm run build:grammar` after editing;
// `npm test` fails if the committed JSON is out of date.
//
// Every rule here mirrors src/language/scanner.ts, parser.ts and src/render.ts:
// - instruction keywords are case-insensitive and may be indented;
// - a comment is a line whose first non-blank character is `#` (also inside a
//   continuation); `#` elsewhere is text;
// - a line ending in an odd number of `\` continues the instruction;
// - only COPY, RENDER and TRANSFORM scan for `<<NAME` / `<<-NAME` (NAME is an
//   identifier, unquoted); the body ends at a line that is exactly NAME
//   (`<<-` strips leading tabs, not spaces);
// - RENDER bodies are templates (`${…}`, `${#each}`, `${#if}`, `$${`); COPY and
//   TRANSFORM bodies are raw bytes, so `${…}` there is not highlighted.

/** Keyword lists, checked against the parser by test/keywords.test.mjs. */
export const KEYWORDS = {
  instructions: ['FROM', 'STAGE', 'ARG', 'LET', 'IMPORT', 'COPY', 'RENDER', 'TRANSFORM', 'PLUGIN', 'TOOL', 'SITEMAP', 'FEED', 'REDIRECT', 'HEADER', 'FINGERPRINT', 'CHECKLINKS', 'EXPORT'],
  unsupported: ['RUN', 'REMOVE', 'GENERATE', 'ASSERT', 'META', 'EXEC'],
  clauses: ['FROM', 'AS', 'TO', 'INTEGRITY', 'SORT', 'BY', 'ASC', 'DESC', 'SCHEMA', 'DRAFTS', 'PAGE', 'FIRST', 'FOR', 'IN', 'WITH', 'OPTIONS', 'ARGS', 'BASE', 'AT', 'COMPRESSION', 'LINK', 'TITLE', 'STATUS', 'IMAGE', 'SECRET', 'HEADER', 'BEARER', 'MANIFEST', 'ARG'],
  scratch: 'scratch',
  valueTypes: ['string', 'number', 'boolean'],
  importKinds: ['file', 'text', 'json', 'tree', 'markdown', 'yaml', 'csv', 'wbn', 'sitefile'],
  exportFormats: ['directory', 'zip', 'tar', 'tgz', 'tar.gz', 'wbn', 'oci'],
  compressions: ['gzip', 'none'],
  escapeModes: ['none', 'html'],
  redirectStatuses: ['301', '302', '303', '307', '308'],
  flags: {
    COPY: ['overwrite', 'from', 'exclude', 'chmod', 'parents'],
    RENDER: ['escape', 'overwrite', 'from'],
    TRANSFORM: ['overwrite', 'from', 'include', 'exclude'],
    SITEMAP: ['overwrite'],
    FEED: ['overwrite'],
    REDIRECT: ['overwrite'],
    HEADER: ['overwrite'],
    FINGERPRINT: ['overwrite'],
    CHECKLINKS: ['warn', 'external'],
  },
};

/** Heredoc delimiter names that embed another language (matched case-insensitively). */
export const EMBEDS = [
  { names: ['HTML', 'HTM'], scope: 'text.html.basic', lang: 'html' },
  { names: ['CSS'], scope: 'source.css', lang: 'css' },
  { names: ['JAVASCRIPT', 'JS', 'MJS'], scope: 'source.js', lang: 'javascript' },
  { names: ['MARKDOWN', 'MD'], scope: 'text.html.markdown', lang: 'markdown' },
  { names: ['JSON'], scope: 'source.json', lang: 'json' },
  { names: ['YAML', 'YML'], scope: 'source.yaml', lang: 'yaml' },
  { names: ['SVG', 'XML'], scope: 'text.xml', lang: 'xml' },
];

const IDENT = '[A-Za-z_][A-Za-z0-9_]*';
const REF = `${IDENT}(?:\\.[A-Za-z0-9_-]+)*`;
/** Start of a word (start of line or after whitespace). */
const W = '(?<![^\\s])';
/** End of a word. */
const WE = '(?=\\s|$)';
const ci = (words) => `(?i:${words.map(escapeRe).join('|')})`;
function escapeRe(s) {
  return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}
/**
 * End of an instruction: end of line unless it ends in `\` (continuation).
 * Blank lines inside a continuation do not end it; a `\` followed by trailing
 * spaces, or an even `\\`, does. (vscode-textmate appends `\n` to each line.)
 */
const INSTRUCTION_END = '(?<=[^\\s\\\\])\\s*$|(?<=\\\\)[ \\t]+$|(?<=[^\\\\]\\\\\\\\)$';

const S = (name) => `${name}.sitefile`;
const kw = (words, name = S('keyword.other.clause')) => ({ match: `${W}(${ci(words)})${WE}`, captures: { 1: { name } } });

function instruction(word, patterns, extra = {}) {
  return {
    name: S(`meta.instruction.${word.toLowerCase()}`),
    begin: `^\\s*(${ci([word])})${WE}`,
    beginCaptures: { 1: { name: S('keyword.control.instruction') } },
    end: INSTRUCTION_END,
    patterns: [{ include: '#continuation' }, { include: '#continuation-comment' }, ...patterns],
    ...extra,
  };
}

function flags(instr) {
  const known = KEYWORDS.flags[instr];
  const pats = [];
  if (known.includes('escape')) {
    pats.push({
      match: `${W}(--)(escape)(=)(?:(${ci(KEYWORDS.escapeModes)})${WE}|(\\S*))`,
      captures: {
        1: { name: S('punctuation.definition.flag') },
        2: { name: S('variable.parameter.flag') },
        3: { name: S('keyword.operator.assignment') },
        4: { name: S('constant.language.escape-mode') },
        5: { name: S('invalid.illegal.escape-mode') },
      },
    });
  }
  if (known.includes('from')) {
    pats.push({
      // --from=<stage>, or --from=<import>+<stage> (a stage of a Sitefile imported AS sitefile)
      match: `${W}(--)(from)(=)(?:(?:(${IDENT})(\\+))?(${IDENT})${WE}|(\\S*))`,
      captures: {
        1: { name: S('punctuation.definition.flag') },
        2: { name: S('variable.parameter.flag') },
        3: { name: S('keyword.operator.assignment') },
        4: { name: S('variable.other.readwrite.import') },
        5: { name: S('punctuation.separator.imported-stage') },
        6: { name: S('entity.name.section.stage') },
        7: { name: S('invalid.illegal.stage-name') },
      },
    });
  }
  if (known.includes('chmod')) {
    pats.push({
      match: `${W}(--)(chmod)(=)(?:(0?(?:644|755))${WE}|(\\S*))`,
      captures: {
        1: { name: S('punctuation.definition.flag') },
        2: { name: S('variable.parameter.flag') },
        3: { name: S('keyword.operator.assignment') },
        4: { name: S('constant.numeric.mode') },
        5: { name: S('invalid.illegal.mode') },
      },
    });
  }
  const patterns = known.filter((f) => f === 'exclude' || f === 'include');
  if (patterns.length) {
    pats.push({
      match: `${W}(--)(${patterns.join('|')})(=)(\\S+)`,
      captures: {
        1: { name: S('punctuation.definition.flag') },
        2: { name: S('variable.parameter.flag') },
        3: { name: S('keyword.operator.assignment') },
        4: { name: S('string.unquoted.glob') },
      },
    });
  }
  const bare = known.filter((f) => !['escape', 'from', 'chmod', 'exclude', 'include'].includes(f));
  if (bare.length) {
    pats.push({
      match: `${W}(--)(${bare.join('|')})${WE}`,
      captures: { 1: { name: S('punctuation.definition.flag') }, 2: { name: S('variable.parameter.flag') } },
    });
  }
  pats.push({ match: `${W}--\\S*`, name: S('invalid.illegal.unknown-flag') });
  return pats;
}

/** `<<NAME` / `<<-NAME`: one rule per (strip, embedded language), plus a generic one. */
function heredocs(mode, header) {
  const out = [];
  const tmpl = mode === 'template';
  for (const strip of [true, false]) {
    for (const e of [...EMBEDS, undefined]) {
      const delim = e ? ci(e.names) : IDENT;
      const tabs = strip ? '\\t*' : '';
      const body = e
        ? {
            // `while` re-checks every line, so a construct the embedded grammar
            // leaves open (an unclosed tag, a comment) cannot swallow the
            // closing line. The outer `end` (a backreference) is exact.
            begin: `^(?!${tabs}${delim}$)`,
            while: `^(?!${tabs}${delim}$)`,
            contentName: `meta.embedded.block.${e.lang}`,
            patterns: [...(tmpl ? [{ include: '#template' }] : []), { include: e.scope }],
          }
        : {
            match: '^.+$',
            name: S('string.unquoted.heredoc'),
            ...(tmpl ? { captures: { 0: { patterns: [{ include: '#template' }] } } } : {}),
          };
      out.push({
        name: S(`meta.heredoc.${mode}`),
        begin: `(?<=\\s)(<<${strip ? '-' : ''})(${delim})${WE}`,
        beginCaptures: { 1: { name: S('keyword.operator.heredoc') }, 2: { name: S('entity.name.tag.heredoc') } },
        end: `^${tabs}(\\2)$`,
        endCaptures: { 1: { name: S('entity.name.tag.heredoc') } },
        patterns: [
          // The rest of the instruction line after the marker (`TO /index.html`).
          { name: S('meta.heredoc.header'), begin: '\\G(?=.)', end: '(?<!\\\\)$', patterns: [{ include: '#continuation' }, ...header] },
          body,
        ],
      });
    }
  }
  return out;
}

const forClause = {
  match: `${W}(${ci(['FOR'])})\\s+(${IDENT})\\s+(${ci(['IN'])})\\s+(${IDENT})${WE}`,
  captures: {
    1: { name: S('keyword.other.clause') },
    2: { name: S('variable.other.definition.loop') },
    3: { name: S('keyword.other.clause') },
    4: { name: S('variable.other.readwrite') },
  },
};
const integrity = {
  match: `${W}(${ci(['INTEGRITY'])})(?:\\s+(?:(sha256:[0-9a-f]{64})${WE}|(\\S+)))?`,
  captures: { 1: { name: S('keyword.other.clause') }, 2: { name: S('constant.other.integrity') }, 3: { name: S('invalid.illegal.integrity') } },
};
const valueAfter = (word, values, okName, badName) => ({
  match: `${W}(${ci([word])})(?:\\s+(?:(${ci(values)})${WE}|([^\\s\\\\]\\S*)))?`,
  captures: { 1: { name: S('keyword.other.clause') }, 2: { name: S(okName) }, 3: { name: S(badName) } },
});
const head = (name) => ({ match: `\\G\\s+(${IDENT})${WE}`, captures: { 1: { name: S(name) } } });
/** A bare identifier base (not `scratch`) names an earlier stage. */
/** A stage name, or `<import>+<stage>`: a stage of a Sitefile imported AS sitefile (issue #97). */
const stageBase = (prefix) => ({
  match: `${prefix}\\s+(?!${KEYWORDS.scratch}${WE})(?:(${IDENT})(\\+))?(${IDENT})${WE}`,
  captures: {
    1: { name: S('keyword.other.clause') },
    2: { name: S('variable.other.readwrite.import') },
    3: { name: S('punctuation.separator.imported-stage') },
    4: { name: S('entity.name.section.stage') },
  },
});
const scratch = { match: `${W}(${KEYWORDS.scratch})${WE}`, captures: { 1: { name: S('constant.language.scratch') } } };
/** `image <ref>`: `image` is the keyword only when a word other than AS follows it (else a stage or directory). */
const imageBase = (prefix) => ({
  match: `${prefix}\\s+(${ci(['image'])})(?=\\s+(?!${ci(['AS'])}${WE})[^\\s\\\\])`,
  captures: { 1: { name: S('keyword.other.clause') }, 2: { name: S('keyword.other.clause') } },
});

// FOR item IN list [BY key] [PAGE n [FIRST <dest>]]
const byClause = { match: `${W}(${ci(['BY'])})\\s+(${IDENT})${WE}`, captures: { 1: { name: S('keyword.other.clause') }, 2: { name: S('variable.other.property.group-key') } } };
const renderClauses = [forClause, byClause, kw(['TO', 'FOR', 'IN', 'PAGE', 'FIRST']), { include: '#scalar' }, { include: '#operands' }];
const transformClauses = [
  forClause,
  { match: `${W}(${ci(['WITH'])})\\s+(${IDENT})${WE}`, captures: { 1: { name: S('keyword.other.clause') }, 2: { name: S('entity.name.function.plugin') } } },
  kw(['WITH', 'TO', 'OPTIONS', 'FOR', 'IN', 'ARGS']),
  { include: '#option' },
  { include: '#placeholder' },
  { include: '#scalar' },
  { include: '#operands' },
];

export function buildGrammar() {
  return {
    $schema: 'https://raw.githubusercontent.com/martinring/tmlanguage/master/tmlanguage.json',
    name: 'Sitefile',
    scopeName: 'source.sitefile',
    fileTypes: ['Sitefile', 'sitefile'],
    patterns: [{ include: '#directive' }, { include: '#lint-comment' }, { include: '#comment' }, { include: '#instructions' }],
    repository: {
      'heredoc-copy': { patterns: heredocs('raw', [{ include: '#operands' }]) },
      'heredoc-render': { patterns: heredocs('template', [{ include: '#render-clauses' }]) },
      'heredoc-transform': { patterns: heredocs('raw', [{ include: '#transform-clauses' }]) },
      'render-clauses': { patterns: renderClauses },
      'transform-clauses': { patterns: transformClauses },
      // `# sitefile=1`: the syntax version directive (scanner.ts VERSION_DIRECTIVE).
      directive: {
        match: '^\\s*(#)[ \\t]*((?i:sitefile))[ \\t]*(=)[ \\t]*(?:([1-9][0-9]*)[ \\t]*$|(.*)$)',
        name: S('comment.line.directive'),
        captures: {
          1: { name: S('punctuation.definition.comment') },
          2: { name: S('keyword.other.directive') },
          3: { name: S('keyword.operator.assignment') },
          4: { name: S('constant.numeric.version') },
          5: { name: S('invalid.illegal.version') },
        },
      },
      // `# sitefile-lint: disable=<rule>,…` / `disable-next-line=…` (src/lint.ts).
      'lint-comment': {
        match: '^\\s*(#)[ \\t]*((?i:sitefile-lint))[ \\t]*(:)[ \\t]*(?:((?:disable-next-line|disable))[ \\t]*(=)(.*))?.*$',
        name: S('comment.line.directive'),
        captures: {
          1: { name: S('punctuation.definition.comment') },
          2: { name: S('keyword.other.directive') },
          3: { name: S('punctuation.separator.key-value') },
          4: { name: S('keyword.other.directive') },
          5: { name: S('keyword.operator.assignment') },
          6: { name: S('constant.other.rule') },
        },
      },
      comment: { match: '^\\s*(#).*$', name: S('comment.line.number-sign'), captures: { 1: { name: S('punctuation.definition.comment') } } },
      'continuation-comment': {
        match: '^\\s*(#).*\\n?',
        name: S('comment.line.number-sign'),
        captures: { 1: { name: S('punctuation.definition.comment') } },
      },
      continuation: { match: '\\\\$', name: S('constant.character.escape.line-continuation') },
      instructions: {
        patterns: [
          instruction('FROM', [scratch, imageBase('\\G()'), stageBase('\\G()'), kw(['AT']), { match: `${W}(${ci(['AS'])})(?:\\s+(${IDENT})${WE})?`, captures: { 1: { name: S('keyword.other.clause') }, 2: { name: S('entity.name.section.stage') } } }, { include: '#operands' }]),
          instruction('STAGE', [head('entity.name.section.stage'), imageBase(`${W}(${ci(['FROM'])})`), stageBase(`${W}(${ci(['FROM'])})`), kw(['FROM', 'AT']), scratch, { include: '#operands' }]),
          ...['ARG', 'LET'].map((w) =>
            instruction(w, [
              { match: `\\G\\s+(${IDENT})(?=\\s|$|[:=])`, captures: { 1: { name: S('variable.other.definition') } } },
              {
                match: `(:)\\s*(?:(${ci(KEYWORDS.valueTypes)})(?=\\s|$|=)|([^\\s=]*))`,
                captures: { 1: { name: S('punctuation.separator.type') }, 2: { name: S('support.type.primitive') }, 3: { name: S('invalid.illegal.type') } },
              },
              { match: '=', name: S('keyword.operator.assignment') },
              { include: '#scalar' },
              { include: '#operands' },
            ]),
          ),
          instruction('IMPORT', [
            head('variable.other.definition.import'),
            kw(['FROM']),
            valueAfter('AS', KEYWORDS.importKinds, 'support.type.import-kind', 'invalid.illegal.import-kind'),
            integrity,
            {
              match: `${W}(${ci(['SORT'])})\\s+(${ci(['BY'])})\\s+(${IDENT})(?:\\s+(${ci(['ASC', 'DESC'])}))?${WE}`,
              captures: {
                1: { name: S('keyword.other.clause') },
                2: { name: S('keyword.other.clause') },
                3: { name: S('variable.other.property.sort-key') },
                4: { name: S('keyword.other.clause') },
              },
            },
            kw(['SORT', 'BY', 'ASC', 'DESC', 'SCHEMA', 'DRAFTS']),
            {
              // SECRET <id> AS HEADER <Name> | SECRET <id> AS BEARER (build secrets)
              match: `${W}(${ci(['SECRET'])})\\s+(${IDENT})\\s+(${ci(['AS'])})\\s+(?:(${ci(['BEARER'])})${WE}|(${ci(['HEADER'])})\\s+(\\S+))`,
              captures: {
                1: { name: S('keyword.other.clause') },
                2: { name: S('variable.other.secret') },
                3: { name: S('keyword.other.clause') },
                4: { name: S('keyword.other.clause') },
                5: { name: S('keyword.other.clause') },
                6: { name: S('entity.name.tag.header') },
              },
            },
            kw(['SECRET', 'HEADER', 'BEARER']),
            {
              // ARG NAME=value: a build arg of the Sitefile imported AS sitefile (issue #97)
              match: `${W}(${ci(['ARG'])})\\s+(${IDENT})\\s*(=)`,
              captures: {
                1: { name: S('keyword.other.clause') },
                2: { name: S('variable.other.arg') },
                3: { name: S('keyword.operator.assignment') },
              },
            },
            kw(['ARG']),
            { include: '#scalar' },
            { include: '#operands' },
          ]),
          instruction('COPY', [...flags('COPY'), { include: '#heredoc-copy' }, { include: '#operands' }]),
          instruction('RENDER', [...flags('RENDER'), { include: '#heredoc-render' }, { include: '#render-clauses' }]),
          instruction('TRANSFORM', [...flags('TRANSFORM'), { include: '#heredoc-transform' }, { include: '#transform-clauses' }]),
          instruction('PLUGIN', [head('entity.name.function.plugin'), kw(['FROM']), integrity, { include: '#operands' }]),
          instruction('TOOL', [head('entity.name.function.tool')]),
          instruction('SITEMAP', [...flags('SITEMAP'), kw(['BASE', 'TO']), { include: '#operands' }]),
          instruction('FINGERPRINT', [...flags('FINGERPRINT'), kw(['MANIFEST']), { include: '#operands' }]),
          instruction('CHECKLINKS', [...flags('CHECKLINKS')]),
          instruction('FEED', [
            // FEED [--overwrite] <import> BASE … LINK … [TO …] [TITLE …]
            {
              match: `\\G((?:\\s+--\\S+)*)\\s+(${IDENT})${WE}`,
              captures: { 1: { patterns: flags('FEED') }, 2: { name: S('variable.other.readwrite.import') } },
            },
            ...flags('FEED'),
            kw(['BASE', 'LINK', 'TO', 'TITLE']),
            { include: '#operands' },
          ]),
          instruction('REDIRECT', [
            ...flags('REDIRECT'),
            valueAfter('STATUS', KEYWORDS.redirectStatuses, 'constant.numeric.status', 'invalid.illegal.status'),
            { include: '#operands' },
          ]),
          instruction('HEADER', [
            ...flags('HEADER'),
            // HEADER <pattern> Name: value…
            {
              match: `${W}([!#$%&'*+.^_\`|~0-9A-Za-z-]+)(:)${WE}`,
              captures: { 1: { name: S('entity.other.attribute-name.header') }, 2: { name: S('punctuation.separator.header') } },
            },
            { include: '#operands' },
          ]),
          instruction('EXPORT', [
            valueAfter('AS', KEYWORDS.exportFormats, 'support.type.export-format', 'invalid.illegal.export-format'),
            valueAfter('COMPRESSION', KEYWORDS.compressions, 'constant.language.compression', 'invalid.illegal.compression'),
            kw(['BASE', 'AT']),
            { include: '#operands' },
          ]),
          {
            // Reserved: the parser rejects these with E_UNSUPPORTED (RUN never runs a shell).
            match: `^\\s*(${ci(KEYWORDS.unsupported)})${WE}`,
            captures: { 1: { name: S('invalid.illegal.unsupported-instruction') } },
          },
        ],
      },
      operands: {
        patterns: [
          { include: '#string-double' },
          { include: '#string-single' },
          { include: '#url' },
          { include: '#resource-ref' },
          { include: '#path' },
          { include: '#interpolation' },
          { include: '#escape' },
        ],
      },
      scalar: {
        patterns: [
          { match: `(?<![^\\s=])-?[0-9]+(?:\\.[0-9]+)?${WE}`, name: S('constant.numeric') },
          { match: `(?<![^\\s=])(?i:true|false)${WE}`, name: S('constant.language.boolean') },
        ],
      },
      'string-double': {
        name: S('string.quoted.double'),
        begin: '(?<![^\\s=])"',
        beginCaptures: { 0: { name: S('punctuation.definition.string.begin') } },
        end: '"|(?<!\\\\)$',
        endCaptures: { 0: { name: S('punctuation.definition.string.end') } },
        patterns: [{ include: '#continuation' }, { include: '#escape' }, { include: '#interpolation' }],
      },
      'string-single': {
        // Literal: no escapes, no interpolation.
        name: S('string.quoted.single'),
        begin: "(?<![^\\s=])'",
        beginCaptures: { 0: { name: S('punctuation.definition.string.begin') } },
        end: "'|(?<!\\\\)$",
        endCaptures: { 0: { name: S('punctuation.definition.string.end') } },
        patterns: [{ include: '#continuation' }],
      },
      escape: { match: '\\\\[^\\n]', name: S('constant.character.escape') },
      url: {
        match: `(?<![^\\s=])(?:[Gg][Ii][Tt]\\+)?https?://(?:[^\\s\\\\]|\\\\[^\\n])*`,
        name: S('markup.underline.link'),
        captures: { 0: { patterns: [{ include: '#interpolation' }] } },
      },
      'resource-ref': {
        // `name:` / `name:/path` (an IMPORT used as a COPY source).
        match: `${W}(${IDENT})(:)(?=/|\\s|$)`,
        captures: { 1: { name: S('variable.other.readwrite.import') }, 2: { name: S('punctuation.separator.resource') } },
      },
      path: {
        match: `(?<![^\\s=:}])(?:\\.\\.?)?/(?:[^\\s\\\\]|\\\\[^\\n])*`,
        name: S('string.unquoted.path'),
        captures: { 0: { patterns: [{ include: '#interpolation' }, { include: '#escape' }] } },
      },
      interpolation: {
        patterns: [
          {
            match: `(\\$\\{)(${REF})(\\})`,
            name: S('meta.interpolation'),
            captures: {
              1: { name: S('punctuation.definition.interpolation.begin') },
              2: { patterns: [{ include: '#ref' }] },
              3: { name: S('punctuation.definition.interpolation.end') },
            },
          },
          { match: '\\$\\{[^}\\s]*\\}?', name: S('invalid.illegal.interpolation') },
        ],
      },
      ref: {
        patterns: [
          { match: '(\\.)([A-Za-z0-9_-]+)', captures: { 1: { name: S('punctuation.accessor') }, 2: { name: S('variable.other.property') } } },
          { match: IDENT, name: S('variable.other.readwrite') },
        ],
      },
      option: {
        // TRANSFORM … OPTIONS key=value
        match: `${W}(${IDENT})(=)`,
        captures: { 1: { name: S('entity.other.attribute-name.option') }, 2: { name: S('keyword.operator.assignment') } },
      },
      placeholder: {
        // TRANSFORM … ARGS {input} {output} {option.key}
        match: '\\{(?:input|output|option\\.[A-Za-z0-9_-]+)\\}',
        name: S('variable.language.placeholder'),
      },
      template: {
        // RENDER template syntax (src/render.ts).
        patterns: [
          { match: '\\$\\$\\{', name: S('constant.character.escape.template') },
          {
            match: `(\\$\\{)(#each)\\s+(${REF})\\s+(as)\\s+(${IDENT})\\s*(\\})`,
            name: S('meta.template.block'),
            captures: {
              1: { name: S('punctuation.definition.template.begin') },
              2: { name: S('keyword.control.loop') },
              3: { patterns: [{ include: '#ref' }] },
              4: { name: S('keyword.control.loop') },
              5: { name: S('variable.other.definition.loop') },
              6: { name: S('punctuation.definition.template.end') },
            },
          },
          {
            match: `(\\$\\{)(#if)\\s+(!?)\\s*(${REF})\\s*(\\})`,
            name: S('meta.template.block'),
            captures: {
              1: { name: S('punctuation.definition.template.begin') },
              2: { name: S('keyword.control.conditional') },
              3: { name: S('keyword.operator.logical') },
              4: { patterns: [{ include: '#ref' }] },
              5: { name: S('punctuation.definition.template.end') },
            },
          },
          {
            match: '(\\$\\{)(else|/each|/if)(\\})',
            name: S('meta.template.block'),
            captures: {
              1: { name: S('punctuation.definition.template.begin') },
              2: { name: S('keyword.control.template') },
              3: { name: S('punctuation.definition.template.end') },
            },
          },
          {
            match: `(\\$\\{)(${REF})(?:(\\|)(raw))?(\\})`,
            name: S('meta.interpolation'),
            captures: {
              1: { name: S('punctuation.definition.interpolation.begin') },
              2: { patterns: [{ include: '#ref' }] },
              3: { name: S('keyword.operator.pipe') },
              4: { name: S('support.function.filter') },
              5: { name: S('punctuation.definition.interpolation.end') },
            },
          },
          // Anything else inside ${…} is E_TEMPLATE at build time.
          { match: '\\$\\{[^}\\n]*\\}', name: S('invalid.illegal.template') },
        ],
      },
    },
  };
}

/**
 * Injected into RENDER heredoc bodies so `${…}` is highlighted even inside an
 * embedded language's own strings and tags (e.g. `<a href="${url}">`).
 */
export function buildInjection() {
  return {
    $schema: 'https://raw.githubusercontent.com/martinring/tmlanguage/master/tmlanguage.json',
    scopeName: 'sitefile.template.injection',
    injectionSelector: 'L:meta.heredoc.template.sitefile -meta.heredoc.header.sitefile',
    patterns: [{ include: 'source.sitefile#template' }],
  };
}

export const embeddedLanguages = Object.fromEntries(EMBEDS.map((e) => [`meta.embedded.block.${e.lang}`, e.lang]));

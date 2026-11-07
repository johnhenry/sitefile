// Regenerates the Sitefile parser differential fixtures from the TypeScript reference.
//
//   npm ci && npm run build        # at the repository root (builds lib/)
//   node rust/tests/fixtures/sitefile/generate.mjs
//
// Writes, next to this script:
//   fuzz.json         seeded inputs [{input, parse, plan}] (format: record.mjs)
//   conformance.json  `sitefile validate` (TypeScript CLI) on every Sitefile under
//                     conformance/cases and examples: [{dir, args, code, stdout, stderr}]
// Checked by the Rust tests `language::differential` (parser, validator, planner,
// parse_sitefile_all and LSP diagnostics) and tests/lsp_parity.rs (Rust CLI and
// LSP against conformance.json), and by test/parser-fuzz-fixture.test.ts
// (the TypeScript side still produces fuzz.json).
//
// For a larger local run (not committed):
//   FUZZ_OUT=/tmp/sf-fuzz.json FUZZ_COUNT=50000 FUZZ_SEED=7 node rust/tests/fixtures/sitefile/generate.mjs
//   (FUZZ_NOISE=0.3 makes 30% of the inputs pure noise; default 0.08)
//   SITEFILE_PARSER_FUZZ=/tmp/sf-fuzz.json cargo test --release -- --ignored differential_external
//
// New instructions: add a generator to INSTRUCTIONS (and its keywords to WORDS).
import { spawnSync } from 'node:child_process';
import { mkdtempSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { loadLib, placeholder, record } from './record.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '../../../..');
const lib = await loadLib(root);

// ---------------------------------------------------------------- random

// Seeded: mulberry32.
let seed = Number(process.env.FUZZ_SEED ?? 0x5173f11e);
const rand = () => {
  seed = (seed + 0x6d2b79f5) | 0;
  let t = seed;
  t = Math.imul(t ^ (t >>> 15), t | 1);
  t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
};
const int = (n) => Math.floor(rand() * n);
const pick = (xs) => xs[int(xs.length)];
const chance = (p) => rand() < p;
const maybe = (s, p = 0.5) => (chance(p) ? (typeof s === 'function' ? s() : s) : '');

// ---------------------------------------------------------------- vocabulary

const NAMES = ['a', 'b', 'site', 'posts', 'TITLE', 'x', 'out', 'md', 'og', 'cp', 'post', 'build', '_n', 'A1', 'data', 'item'];
const TYPES = ['string', 'number', 'boolean', 'String', 'NUMBER', 'int', 'bool', 'html', 'HTML'];
const KINDS = ['file', 'text', 'json', 'tree', 'markdown', 'yaml', 'csv', 'wbn', 'JSON', 'xml', 'html'];
/** Templates of html fragments (issue #106): valid, broken, using names, loop variables and other fragments. */
const FRAGMENTS = [
  '<b>${TITLE}</b>', '<a href="/${item.slug}">${item.title}</a>', '${#each posts as p}<i>${p.title}</i>${/each}', '${#if x}y${else}z${/if}',
  '${a}${b}', '${frag}', '${TITLE.x}', '${#each}', '${x y}', '<p>', '', '$${literal}', '${posts|raw}', 'a <<EOF b', '${x', '<i>${n}</i>',
  // `${file …}` helpers (issue #105).
  '${file "/img/${item.slug}.png" | img alt="${TITLE}"}', '${file /a.css | stylesheet}', '${file x | bogus}', '${file "/a" | img alt="${frag}"}',
  '${file "/a" | img alt="${TITLE.x}"}', '${file "/a" | width x}', '${file "/img/${item.slug}.json" | srcset}', '${file /a | srcset x}', '${file "/a}', "${file '/a' | svg class=a}", '${file }', '${file "/a" | img alt=${B}}',
];
const FORMATS = ['directory', 'zip', 'tar', 'tgz', 'tar.gz', 'wbn', 'oci', 'OCI', 'rar', 'Tar.GZ', 'ZİP', 'DİRECTORY'];
const DIGEST = 'sha256:' + 'ab'.repeat(32);
const DIGESTS = [DIGEST, DIGEST.toUpperCase(), 'sha256:abc', 'md5:x', '"' + DIGEST + '"', 'sha256:${x}'];
const PATHS = [
  './src', '/index.html', '/', './style.css', 'style.css', '/a/b/', './a b', '../up', '/../x', '/a/./b', 'C:\\x', '/x//y',
  'posts:', 'site:/about.html', 'data:', 'md:posts', 'https://example.com/a.json', 'http://x/y', 'file:///etc/passwd', '//host/x',
  '/a/${TITLE}.html', '/${x}', '"/q uoted.html"', "'/single ${x}'", '"/dq ${TITLE}/x"', '/p/${post.slug}.html', '/p/${item}/index.html',
  '"a\\"b"', '"\\n\\t"', 'a\\ b', '~', '.', '..', '/🙂.html', '/é', '/a?b#c', '"${a}${b}"', '${x}', '""', "''",
  '/a/../../x', '/./', '/a\\\\b', '"/a\\\\b"', '/%2e%2e/x', '"/x\\u0000"', '/a/b/..', './a/../..', 'a/./b/', '/CON', '/a:b', '/ä/Ä', '/ x', '"/ "',
];
const NUMBERS = ['0', '1', '-0', '1e3', '1E3', '0x10', '0b1', '0o7', '.5', '5.', '1_000', 'Infinity', '-Infinity', 'NaN', '1e400', '9007199254740993', '+1', '0.1', '1.50', '1e21', '1e-7', '00', '" 5 "', '""', '"\\t1\\n"', 'abc', '123456789012345678901234', '-', '2e-324'];
const BOOLEANS = ['true', 'false', 'TRUE', 'True', '1', '0', 'yes', '""', '" true"'];
const EXCLUDES = ['*.md', 'drafts/', '!keep.md', '/css/x.css', '**/*.map', '#c', '/', '!', 'a\\ b', '${x}'];
const MODES = ['644', '0644', '755', '0755', '600', '0o755', '777', 'x'];
const VALUES = ['hello', '"Hello world"', "'single'", '"${TITLE} x"', '${x}', '""', 'é', '"tab\\there"', ...NUMBERS.slice(0, 12), ...BOOLEANS.slice(0, 5)];

/** `FROM image <ref> [AT <dir>]` references: registry, local-store, digest-pinned and invalid ones. */
const IMAGES = ['ghcr.io/me/theme:1.2', 'theme:1', 'theme', `localhost:5000/t@sha256:${'0'.repeat(64)}`, 'Bad:1', '${x}', 'reg.example/a/b:${x}', '"quoted:1"', 'AT', 'a b'];
const imageBase = () => `${pick(['image', 'IMAGE', 'Image'])} ${pick(IMAGES)}${maybe(() => ` AT ${pick(['/srv/www', '/', 'rel', '/a/../b', '${x}', ''])}`, 0.3)}`;

/** Instruction lines (one string; heredoc bodies use \n). Add new instructions here. */
const INSTRUCTIONS = {
  FROM: () => `FROM ${chance(0.15) ? imageBase() : pick(['scratch', 'scratch', 'image', ...NAMES, ...PATHS])}${maybe(() => ` AS ${pick(NAMES)}`)}`,
  STAGE: () => `STAGE ${pick(NAMES)}${maybe(() => ` FROM ${chance(0.15) ? imageBase() : pick(['scratch', 'image', ...NAMES, './src', '/x'])}`)}`,
  ARG: () => `ARG ${pick(NAMES)}${maybe(() => `${pick([':', ' : ', ': '])}${pick(TYPES)}`, 0.3)}${maybe(() => `${pick(['=', ' = ', '= '])}${pick(VALUES)}`, 0.6)}`,
  LET: () => {
    // An html fragment: one line, or a heredoc (sometimes after `=`).
    if (chance(0.25)) {
      const name = pick([...NAMES, 'frag']);
      const type = pick(['html', 'HTML', 'Html', 'html ']);
      if (chance(0.5)) return `LET ${name}${pick([':', ': ', ' : '])}${type}${pick([' = ', '=', ' ', ' = \\\n '])}${pick(FRAGMENTS)}`;
      const h = heredoc(FRAGMENTS);
      pendingBody += h.tail;
      return `LET ${name}: ${type}${pick([' ', ' = '])}${h.marker}`;
    }
    return `LET ${pick(NAMES)}${maybe(() => `:${pick(TYPES)}`, 0.3)}${maybe(() => `${pick(['=', ' = '])}${pick(VALUES)}`, 0.85)}`;
  },
  IMPORT: () => {
    const clauses = [];
    if (chance(0.6)) clauses.push(`AS ${pick(KINDS)}`);
    if (chance(0.25)) clauses.push(`INTEGRITY ${pick(DIGESTS)}`);
    if (chance(0.25)) clauses.push(`SORT BY ${pick(['date', 'title', 'x-y', '1'])}${maybe(() => ` ${pick(['DESC', 'ASC', 'desc', 'UP'])}`)}`);
    // Build secrets (issue #85), sometimes twice (the same header twice is E_SYNTAX).
    for (let n = chance(0.15) ? 1 + int(2) : 0; n > 0; n--) {
      clauses.push(`SECRET ${pick(['k', 'tok', '1x', 'a-b'])}${pick([' AS BEARER', ' as bearer', ' AS HEADER X-Api-Key', ' AS HEADER Authorization', ' AS HEADER Host', ' AS HEADER a:b', ' AS', ' AS HEADER', ' BEARER', ''])}`);
    }
    if (chance(0.5)) clauses.reverse();
    return `IMPORT ${pick(NAMES)} FROM ${pick(PATHS)}${clauses.map((c) => ' ' + c).join('')}`;
  },
  COPY: () =>
    `COPY ${flags(['--overwrite', '--from=' + pick(NAMES), '--from=', '--from', '--bogus', '--exclude=' + pick(EXCLUDES), '--exclude', '--chmod=' + pick(MODES), '--chmod', '--parents', '--parents=1'])}${source()} ${pick(PATHS)}`,
  RENDER: () =>
    `RENDER ${flags(['--escape=html', '--escape=none', '--escape=HTML', '--escape=js', '--escape', '--overwrite', '--from=' + pick(NAMES), '--inline-svg', '--inline-svg=1'])}${source()} TO ${pick(PATHS)}${maybe(() => ` FOR ${pick(NAMES)} IN ${pick(NAMES)}`, 0.3)}`,
  TRANSFORM: () => {
    const fl = flags(['--overwrite', '--from=' + pick(NAMES), '--include=' + pick(EXCLUDES), '--exclude=' + pick(EXCLUDES), '--include', '--exclude=']);
    let s = `TRANSFORM ${fl}${maybe(() => source() + ' ', 0.8)}WITH ${pick(NAMES)} TO ${pick([...PATHS, '/img/*.png', '/out/*', '/a/*.'])}`;
    if (chance(0.4)) s += ` OPTIONS ${Array.from({ length: 1 + int(3) }, () => `${pick(['w', 'q', 'title', 'x'])}=${pick(VALUES)}`).join(' ')}`;
    if (chance(0.3)) s += ` FOR ${pick(NAMES)} IN ${pick(NAMES)}`;
    if (chance(0.3)) s += ` ARGS ${pick(['-i {input} {output}', '{output}', '{input}', '{option.w} {output}', '"%{pts}" {output}', 'x'])}`;
    return s;
  },
  PLUGIN: () => `PLUGIN ${pick(NAMES)} FROM ${pick(['./plugins/x.mjs', './p.wasm', 'https://e.x/p.mjs', ...PATHS])}${maybe(() => ` INTEGRITY ${pick(DIGESTS)}`, 0.3)}`,
  TOOL: () => `TOOL ${pick(NAMES)}${maybe(' extra', 0.1)}`,
  EXPORT: () =>
    `EXPORT ${pick(['/', '/', '/sub', './x', '/${x}', 'out', '"/"'])}${maybe(() => ` AS ${pick(FORMATS)}`, 0.4)}${maybe(() => ` BASE ${pick(['https://e.x/', 'http://e.x', '${x}'])}`, 0.15)}${maybe(() => ` AT ${pick(['/srv/www', 'rel', '/'])}`, 0.15)}${maybe(() => ` COMPRESSION ${pick(['gzip', 'none', 'NONE', 'zstd'])}`, 0.15)}`,
  SITEMAP: () => `SITEMAP ${flags(['--overwrite', '--bogus'])}${maybe(() => `BASE ${pick(['https://e.x/', 'https://e.x', 'http://e.x/a/', 'ftp://e.x', '${x}', 'e.x', '""'])}`, 0.9)}${maybe(() => ` TO ${pick(PATHS)}`, 0.4)}`,
  FEED: () => {
    let s = `FEED ${flags(['--overwrite'])}${pick(NAMES)}`;
    if (chance(0.9)) s += ` BASE ${pick(['https://e.x', 'https://e.x/', 'e.x', '${x}'])}`;
    if (chance(0.9)) s += ` LINK ${pick(['/p/${item.slug}.html', '/p/${item.slug}/index.html', 'p/${item.slug}', '/${x}', 'https://e.x/a', '/a'])}`;
    if (chance(0.4)) s += ` TO ${pick(PATHS)}`;
    if (chance(0.4)) s += ` TITLE ${pick(['"My & <feed>"', 'Notes', '${x}', '""'])}`;
    if (chance(0.15)) s += ` ${pick(['TITLE x TO /f.xml', 'LINK /a', 'BOGUS'])}`;
    return s;
  },
  REDIRECT: () => `REDIRECT ${flags(['--overwrite'])}${pick(['/old', '/a/*', '/${x}', 'old', '/a b', ...PATHS])} ${pick(['/new', 'https://e.x/:splat', '/b', '', '/c\\n'])}${maybe(() => ` STATUS ${pick(['301', '302', '303', '307', '308', '200', '418', 'x', '301!'])}`, 0.4)}`,
  HEADER: () => `HEADER ${flags(['--overwrite'])}${pick(['/*', '/assets/*', '/a', 'a', '/${x}'])} ${pick(['X-Frame-Options: DENY', 'Cache-Control: public, max-age=60', 'X-A:1', 'bad name: 1', 'X-A: "1\\nSet-Cookie: a"', 'X-A', ': v', 'X-É: 1'])}`,
  FINGERPRINT: () =>
    `FINGERPRINT ${flags(['--overwrite', '--bogus', '--from=a'])}${Array.from({ length: int(4) }, () => pick(['/assets/', '/**/*.css', '/*.js', '/a?.png', 'assets/', '/${x}/', '"/q r/"', '/x\\ty', 'MANIFEST', '/'])).join(' ')}${maybe(() => ` MANIFEST ${pick(['/m.json', '/a/', 'm.json', '${x}', '', '/m.json extra'])}`, 0.4)}`,
  CHECKLINKS: () => `CHECKLINKS ${flags(['--warn', '--external', '--overwrite', '--warn=1'])}${maybe(() => pick(['/a.html', 'x', '--', 'MANIFEST']), 0.1)}`,
  // The syntax version directive (a comment; only valid before the first instruction).
  DIRECTIVE: () => pick(['#', ' #', '\t#']) + pick([' ', '', '  ']) + pick(['sitefile', 'SITEFILE', 'SiteFile', 'sitefile-lint:', 'ſitefile']) + pick(['=1', ' = 1', '=2', '=10', '=x', '=', '=01', '=1.0', '=1 x', '= \u00a01']),
  // Lint comments (`# sitefile-lint: …`), valid or not.
  LINT: () =>
    `${pick(['#', '  #'])} ${pick(['sitefile-lint:', 'SITEFILE-LINT :', 'sitefile-lint'])} ${pick(['disable', 'disable-next-line', 'disabel', 'disable-next-line '])}=${pick(['unused-name', 'render-escape', 'unpinned-import', 'unpinned-image', 'sitemap-order', 'shadowed-write', 'all', 'bogus', 'unused-name, render-escape', ''])}`,
  // Rejected keywords.
  RUN: () => `RUN ${pick(['echo hi', 'make', ''])}`,
  NOT_YET: () => `${pick(['REMOVE', 'GENERATE', 'ASSERT', 'META', 'EXEC'])} ${pick(PATHS)}`,
  UNKNOWN: () => `${pick(['WORKDIR', 'ENV', 'ADD', 'Copy', 'copy', 'FROMX', 'É', '--x', 'ſtage', 'İmport', 'ﬁ', 'exec'])} ${pick(PATHS)}`,
};
/** Relative weights (instructions not listed get 1). */
const WEIGHTS = { COPY: 4, RENDER: 3, ARG: 2, LET: 2, IMPORT: 2, TRANSFORM: 2, RUN: 0.3, NOT_YET: 0.3, UNKNOWN: 0.4, DIRECTIVE: 0.3, LINT: 0.4 };
/** Keywords mutations may splice in. */
const WORDS = [
  'FROM', 'STAGE', 'image', 'IMAGE', 'AS', 'TO', 'FOR', 'IN', 'WITH', 'OPTIONS', 'ARGS', 'INTEGRITY', 'SORT', 'BY', 'BASE', 'AT', 'COMPRESSION', 'scratch', 'EXPORT', 'COPY', 'SITEMAP', 'FEED', 'LINK', 'TITLE', 'REDIRECT', 'STATUS', 'HEADER', 'FINGERPRINT', 'MANIFEST', 'CHECKLINKS',
  // Case mapping that differs between ASCII and Unicode rules.
  'ſtage', 'İmport', 'ſcratch', 'ﬁ', 'COPẎ', 'ΣΑΣ', 'ß', 'Expoʀt',
];

function flags(list) {
  let s = '';
  while (chance(0.25)) s += pick(list) + ' ';
  return s;
}

function heredoc(lines = ['body { color: red }', '<h1>${TITLE}</h1>', '{{#each posts as p}}x{{/each}}', '# not a comment', '', '  indented', 'EOF ', 'COPY a b', '\\', '${frag}']) {
  const delim = pick(['EOF', 'CSS', 'HTML', 'A', '_x1']);
  const strip = chance(0.35);
  const body = Array.from({ length: int(4) }, () => (strip && chance(0.5) ? '\t' : '') + (chance(0.1) ? delim + 'x' : pick(lines)));
  const close = chance(0.9) ? (strip && chance(0.5) ? '\t\t' : '') + delim : pick([delim + ' ', ' ' + delim, delim.toLowerCase(), '']);
  return { marker: `<<${strip ? '-' : ''}${delim}`, tail: '\n' + [...body, close].join('\n') };
}

/** A COPY/RENDER/TRANSFORM source: a path, or a heredoc marker (its body goes after the line). */
let pendingBody = '';
function source() {
  if (!chance(0.3)) return pick(PATHS);
  const h = heredoc();
  pendingBody += h.tail;
  return h.marker;
}

function instruction() {
  const keys = Object.keys(INSTRUCTIONS);
  const total = keys.reduce((n, k) => n + (WEIGHTS[k] ?? 1), 0);
  let r = rand() * total;
  for (const k of keys) {
    r -= WEIGHTS[k] ?? 1;
    if (r <= 0) return generate(k);
  }
  return generate('COPY');
}

function generate(k) {
  pendingBody = '';
  const line = INSTRUCTIONS[k]();
  const body = pendingBody;
  pendingBody = '';
  return line + body;
}

function program() {
  const lines = [];
  if (chance(0.2)) lines.push(generate('DIRECTIVE'));
  for (let n = int(3); n > 0; n--) lines.push(generate('ARG'));
  if (chance(0.6)) lines.push(generate(pick(['FROM', 'STAGE'])));
  for (let n = int(7); n > 0; n--) lines.push(chance(0.1) ? generate(pick(['FROM', 'STAGE'])) : instruction());
  if (chance(0.9)) lines.push(generate('EXPORT'));
  if (chance(0.15)) lines.splice(int(lines.length + 1), 0, pick(['# comment', '   # indented comment', '', '   ', '\t']));
  return lines;
}

/**
 * A program that should mostly validate and plan: names are declared before
 * they are used, so mutations then exercise the validator and planner too.
 */
function validProgram() {
  const lines = [...(chance(0.25) ? [pick(['# sitefile=1', '#sitefile = 1', '# comment\n# sitefile=1'])] : []), 'ARG TITLE = "Hello"'];
  const vars = ['TITLE'];
  const stages = [];
  const lists = [];
  if (chance(0.4)) {
    const ty = pick(['number', 'string', 'boolean']);
    const values = ty === 'number' ? NUMBERS : ty === 'boolean' ? BOOLEANS : VALUES;
    lines.push(`${pick(['ARG', 'ARG', 'LET'])} n:${ty}${chance(0.7) ? ` = ${pick(values)}` : ''}`.replace(/^LET (n:\w+)$/, 'LET $1 = 1'));
    vars.push('n');
  }
  if (chance(0.5)) {
    lines.push('LET slug = "${TITLE}-x"');
    vars.push('slug');
  }
  const v = () => '${' + pick(vars) + '}';
  if (chance(0.5)) {
    lines.push(`IMPORT posts FROM ./posts AS markdown${maybe(' SORT BY date DESC')}`);
    lists.push('posts');
  }
  if (chance(0.3)) {
    lines.push(`IMPORT data FROM ${pick(['./d.json', `https://example.com/d.json INTEGRITY ${DIGEST}`])} AS json`);
    lists.push('data');
  }
  if (chance(0.3)) lines.push('IMPORT site FROM ./site');
  // html fragments (issue #106), used by the inline RENDER bodies below.
  const frags = [];
  if (chance(0.4)) {
    lines.push(pick(['LET frag: html = <b>${TITLE}</b>', 'LET frag: html <<HTML\n<header>${TITLE}</header>\nHTML', 'LET frag: html = <a href="/${item.slug}">${item.title}</a>', 'LET frag: html = ${#each posts as p}${p.title}${/each}']));
    frags.push('frag');
  }
  if (chance(0.2)) {
    lines.push('IMPORT part FROM ./partials/p.html AS html');
    frags.push('part');
  }
  const plugin = chance(0.3);
  if (plugin) lines.push('PLUGIN og FROM ./plugins/og.mjs');
  const tool = chance(0.3);
  if (tool) lines.push('TOOL cp');
  const body = () => {
    const h = heredoc();
    pendingBody += h.tail;
    return h.marker;
  };
  const stage = () => {
    const n = `s${stages.length}`;
    stages.push(n);
    return chance(0.5) ? `STAGE ${n}${maybe(' FROM scratch')}` : `FROM scratch AS ${n}`;
  };
  const ops = [
    () => `COPY ${maybe('--overwrite ')}${pick(['./src', 'style.css', './a\\ b', `"./q ${v()}"`, 'site:/x', 'site:'])} ${pick(['/', '/css/', `/${v()}.css`, '"/x y/"'])}`,
    () => `COPY ${maybe('--exclude=*.md ')}${maybe('--exclude=drafts/ ')}${maybe(() => `--chmod=${pick(['644', '0755'])} `)}${maybe('--parents ')}${pick(['./src', './src/a.css', 'site:/x', 'site:'])} /out/`,
    () => `COPY --chmod=755 ${body()} /run-${int(9)}.sh`,
    () => `COPY ${body()} /inline-${int(9)}.txt`,
    () => `RENDER ${maybe('--escape=html ')}${maybe('--inline-svg ')}${pick(['./t.html', body()])} TO /${pick(['index', v(), 'a/b'])}.html`,
    () => (frags.length ? `RENDER --escape=html <<HTML TO /f-${int(9)}.html\n\${${pick(frags)}}${pick(['', '<main>${TITLE}</main>', '${' + pick(frags) + '}'])}\nHTML` : 'COPY ./g /g'),
    () => (frags.length ? `COPY ./x /\${${pick(frags)}}` : 'COPY ./h /h'),
    () => (lists.length ? `RENDER ./post.html TO /p/\${item.slug}.html FOR item IN ${pick(lists)}` : 'COPY ./x /x'),
    () => (stages.length > 1 ? `COPY --from=${stages[0]} /out /from-${stages[0]}` : 'COPY ./y /y'),
    () => (plugin ? `TRANSFORM ${maybe('./a.png ')}WITH og TO /og.png${maybe(` OPTIONS w=1 title="${v()}"`)}` : 'COPY ./z /z'),
    () => (tool ? `TRANSFORM ./a.css WITH cp TO /b.css ARGS ${pick(['{input} {output}', '-v {input} {output}'])}` : 'COPY ./w /w'),
    () => (plugin ? `TRANSFORM ${maybe('--include=*.jpg ')}${maybe('--exclude=drafts/ ')}${pick(['./photos/', './photos', 'site:/img'])} WITH og TO ${pick(['/img/', '/img/*.png', '/t/*'])}` : 'COPY ./v /v'),
    () => `SITEMAP ${maybe('--overwrite ')}BASE ${pick(['https://e.x/', `https://e.x/${v()}/`])}${maybe(' TO /map.xml')}`,
    () => (lists.includes('posts') ? `FEED posts BASE https://e.x LINK ${pick(['/p/${item.slug}.html', '/p/${item.slug}/index.html'])}${maybe(' TO /atom.xml')}${maybe(` TITLE "${v()} & notes"`)}` : 'COPY ./f /f'),
    () => `REDIRECT ${maybe('--overwrite ')}${pick(['/old', '/blog/*', `/${v()}`])} ${pick(['/new', 'https://e.x/:splat'])}${maybe(() => ` STATUS ${pick(['301', '302', '308'])}`)}`,
    () => `HEADER ${maybe('--overwrite ')}${pick(['/*', '/assets/*'])} ${pick(['X-Frame-Options: DENY', `Cache-Control: public, max-age=${int(99)}`, `X-Title: ${v()}`])}`,
    () => `CHECKLINKS${maybe(' --warn')}${maybe(' --external', 0.2)}`,
    () => `FINGERPRINT ${maybe('--overwrite ')}${pick(['/assets/', '/**/*.css /*.js', `/${v()}/`])}${maybe(() => ` MANIFEST ${pick(['/asset-manifest.json', `/${v()}.json`])}`)}`,
    stage,
  ];
  if (chance(0.5)) lines.push(stage());
  for (let n = 1 + int(6); n > 0; n--) {
    pendingBody = '';
    const l = pick(ops)();
    lines.push(l + pendingBody);
    pendingBody = '';
  }
  if (stages.length && chance(0.6)) lines.push('FROM scratch');
  lines.push(`EXPORT ${pick(['/', '/', '/sub'])}${maybe(() => pick([' AS zip', ' AS tgz', ' AS oci COMPRESSION none', ' AS oci AT /srv/www', ' AS wbn BASE https://e.x/']), 0.3)}`);
  return lines;
}

// ---------------------------------------------------------------- mutations

const ATOMS = [
  ' ', '  ', '\t', '"', "'", '\\', '\\\\', '$', '${', '}', '${}', '${x', '${a.}', '${a.b-c}', '${1}', '$${a}', '\\${a}', '${x.y.z}', '<<', '<<-', '<<EOF',
  '<<-EOF', '--', '--from=', '=', ':', '#', 'é', '😀', '\u00a0', '\u3000', '\ufeff', '\u2028', '\u0085', '\u200b', '\r', '\n', '\r\n', '\\\n', '/', '..',
  ...WORDS, ...NAMES,
];

const words = (line) => line.split(/(\s+)/);

const MUTATIONS = [
  // tokens
  (ls) => edit(ls, (w) => { w.splice(2 * int((w.length + 1) / 2), 2); }),
  (ls) => edit(ls, (w) => { const i = 2 * int((w.length + 1) / 2); w.splice(i, 0, w[i] ?? 'x', ' '); }),
  (ls) => edit(ls, (w) => { const i = 2 * int((w.length + 1) / 2); if (w[i + 2] !== undefined) [w[i], w[i + 2]] = [w[i + 2], w[i]]; }),
  (ls) => edit(ls, (w) => { w[2 * int((w.length + 1) / 2)] = pick(ATOMS); }),
  (ls) => edit(ls, (w) => { w[0] = pick([w[0]?.toLowerCase(), ' ' + w[0], '\t' + w[0], w[0] + 'S']); }),
  // characters
  (ls) => edit(ls, null, (s) => { const i = int(s.length + 1); return s.slice(0, i) + pick(ATOMS) + s.slice(i); }),
  (ls) => edit(ls, null, (s) => { const i = int(s.length); return s.slice(0, i) + s.slice(i + 1); }),
  (ls) => edit(ls, null, (s) => s.replace(/["']/, '')),
  (ls) => edit(ls, null, (s) => s + pick(['"', "'", '\\', ' \\', '${', ' #x', '   '])),
  // whitespace and continuations
  (ls) => edit(ls, null, (s) => s.replace(/ /g, '\t')),
  (ls) => edit(ls, null, (s) => { const sp = [...s.matchAll(/ /g)]; if (!sp.length) return s; const i = pick(sp).index; return s.slice(0, i) + pick([' \\\n   ', '\\\n', ' \\\n\n# c\n  ', ' \\\r\n\t']) + s.slice(i + 1); }),
  (ls) => edit(ls, null, (s) => s + pick([' \\', '\\\\', '\\\\\\'])),
  // unicode and long lines
  (ls) => edit(ls, null, (s) => { const i = int(s.length + 1); return s.slice(0, i) + pick(['é', '😀', '\u00a0', '\u3000', '\ufeff', 'ｊ', '\u2028', '\u0301']) + s.slice(i); }),
  (ls) => edit(ls, (w) => { const i = 2 * int((w.length + 1) / 2); w[i] = (w[i] ?? 'x').repeat(50 + int(300)); }),
  // lines
  (ls) => { ls.splice(int(ls.length), 1); },
  (ls) => { const i = int(ls.length); ls.splice(i, 0, ls[i] ?? 'EXPORT /'); },
  (ls) => { if (ls.length < 2) return; const i = int(ls.length), j = int(ls.length); [ls[i], ls[j]] = [ls[j], ls[i]]; },
  (ls) => { ls.splice(int(ls.length + 1), 0, instruction()); },
  (ls) => { ls.splice(int(ls.length + 1), 0, noise(1 + int(12))); },
];

/** Apply to a random line: `tokens` edits the whitespace-split words, `chars` the text. */
function edit(lines, tokens, chars) {
  if (!lines.length) return;
  const i = int(lines.length);
  if (tokens) {
    const w = words(lines[i]);
    tokens(w);
    lines[i] = w.filter((x) => x !== undefined).join('');
  } else {
    lines[i] = chars(lines[i]);
  }
}

function noise(n) {
  let s = '';
  for (let k = 0; k < n; k++) s += pick(ATOMS);
  return s;
}

function fuzzCase() {
  if (chance(NOISE)) return noise(1 + int(30));
  const valid = chance(0.45);
  const lines = valid ? validProgram() : program();
  for (let n = valid ? int(3) : int(4); n > 0; n--) pick(MUTATIONS)(lines);
  let src = lines.join('\n');
  if (chance(0.15)) src = src.replace(/\n/g, '\r\n');
  if (chance(0.5)) src += pick(['\n', '\n\n', '\r\n', '\\\n', ' ']);
  // Character edits can split a surrogate pair; Rust input is UTF-8, so keep it well-formed.
  return src.toWellFormed();
}

// ---------------------------------------------------------------- fixed cases

const EDGE = [
  '', '\n', 'EXPORT /', 'FROM scratch\nEXPORT /', 'COPY a /b\nEXPORT /', 'ARG X\nEXPORT /${X}', '\ufeffFROM scratch\nEXPORT /',
  'FROM scratch\r\nCOPY a /b\r\nEXPORT /\r\n', 'COPY \\\n  a \\\n  /b\nEXPORT /', 'COPY a \\', '\\', 'COPY a /b \\\\\nEXPORT /',
  'COPY <<EOF /a\nx\nEOF\nEXPORT /', 'COPY <<-EOF /a\n\tx\n\t\tEOF\nEXPORT /', 'COPY <<EOF /a\nx', 'COPY <<A <<B /a\nA\nB',
  'COPY "<<EOF" /a\nEXPORT /', 'RENDER <<H TO /i.html\n{{x}\nH\nEXPORT /', 'RENDER <<H TO /i.html\n{{#each}}\nH\nEXPORT /', 'EXPORT <<X\nX',
  'COPY <<EOF /a\r\nx\r\nEOF\r\nEXPORT /', 'COPY <<EOF /a\nEOF \nEOF\nEXPORT /', 'copy <<eof /a\nx\neof\nEXPORT /',
  'LET a = "${a}"\nEXPORT /', 'LET a = 1\nLET a = 2\nEXPORT /', 'ARG a:number = x\nEXPORT /', 'ARG a:bool\nEXPORT /', 'LET a\nEXPORT /',
  'COPY "${" /a\nEXPORT /', 'COPY ${} /a\nEXPORT /', 'COPY ${a.} /a\nEXPORT /', 'COPY ${a /a\nEXPORT /', 'COPY "abc /a', "COPY 'abc /a",
  'COPY "a"b /c', 'COPY a\\', 'FROM scratch AS 1x\nEXPORT /', 'STAGE\nEXPORT /', 'STAGE a FROM a\nEXPORT /', 'FROM a AS a\nEXPORT /',
  'STAGE a\nSTAGE b FROM a\nCOPY --from=a /x /y\nEXPORT /', 'STAGE a\nCOPY --from=a /x /y\nEXPORT /', 'FROM scratch\nFROM scratch\nEXPORT /',
  'IMPORT p FROM ./posts AS markdown SORT BY date DESC\nRENDER t.html TO /p/${p.slug}.html FOR p IN p\nEXPORT /',
  'IMPORT p FROM ./posts AS markdown\nRENDER t.html TO /p/${post.slug}.html FOR post IN p\nEXPORT /',
  'IMPORT p FROM ./x AS text SORT BY a\nEXPORT /', 'IMPORT p FROM ./x INTEGRITY sha256:' + 'a'.repeat(64) + '\nEXPORT /',
  'PLUGIN p FROM ./p.mjs\nTRANSFORM a WITH p TO /b OPTIONS w=1 w=2\nEXPORT /', 'TOOL cp\nTRANSFORM a WITH cp TO /b ARGS {input}\nEXPORT /',
  'TOOL cp\nTRANSFORM a WITH cp TO /b ARGS {input} {output} {option.q}\nEXPORT /', 'TRANSFORM --from=a WITH p TO /b\nEXPORT /',
  // TRANSFORM over a directory (issue #127): --include/--exclude and where they are refused.
  'PLUGIN p FROM ./p/p.mjs\nTRANSFORM --include=*.jpg --exclude=x/ --include=*.png ./a WITH p TO /b/*.webp\nEXPORT /',
  'PLUGIN p FROM ./p/p.mjs\nTRANSFORM --include=#x ./a WITH p TO /b\nEXPORT /', 'PLUGIN p FROM ./p/p.mjs\nTRANSFORM --exclude=x WITH p TO /b FOR i IN l\nEXPORT /',
  'PLUGIN p FROM ./p/p.mjs\nTRANSFORM --include=x <<EOF WITH p TO /b\nx\nEOF\nEXPORT /', 'PLUGIN p FROM ./p/p.mjs\nTRANSFORM --exclude=x ./a WITH p TO /b FOR i IN l\nEXPORT /',
  'PLUGIN p FROM ./p/p.mjs\nTRANSFORM --include ./a WITH p TO /b\nEXPORT /', 'PLUGIN p FROM ./p/p.mjs\nTRANSFORM --exclude=/ --from=bad! ./a WITH p TO /b\nEXPORT /',
  'EXPORT / AS oci COMPRESSION', 'EXPORT / AS oci COMPRESSION zstd', 'EXPORT / AS', 'EXPORT / AS rar', 'EXPORT /\nEXPORT /', 'EXPORT /../x',
  'COPY 😀 /😀\nEXPORT /x 😀', 'ARG 😀\nEXPORT /', 'ARG a😀\nEXPORT /', 'COPY a /b\u00a0c\nEXPORT /', 'COPY\u3000a /b\nEXPORT /',
  'COPY a /b\u2028EXPORT /', 'RUN echo', 'REMOVE /x', 'WORKDIR /x', 'FROM\nEXPORT /', 'COPY ' + 'x'.repeat(5000) + ' /y\nEXPORT /',
  '# only a comment', '   \n\t\n', 'ARG X\n', 'COPY --overwrite --overwrite a /b\nEXPORT /', 'COPY --from a /b\nEXPORT /',
  '# sitefile=1\nEXPORT /', '# sitefile=2\nEXPORT /', '#sitefile = 1\n#sitefile=1\nEXPORT /', 'EXPORT /\n# sitefile=1', '# sitefile=1.0\nEXPORT /',
  'COPY --exclude=*.md --exclude=x/ --chmod=755 --parents ./a /b/\nEXPORT /', 'COPY --chmod=600 a /b\nEXPORT /', 'COPY --exclude=#x a /b\nEXPORT /',
  'COPY --parents <<EOF /a\nx\nEOF\nEXPORT /', 'COPY --chmod=755 <<EOF /a\nx\nEOF\nEXPORT /', 'COPY --exclude=a --exclude=a ./a /b\nEXPORT /',
  // Lint rules (src/lint.ts) and their disable comments.
  'ARG A\nLET B = 1\nIMPORT t FROM ./t.json AS json\nIMPORT d FROM ./d\nPLUGIN p FROM ./p/p.mjs\nTOOL cp\nEXPORT /',
  'ARG A\nRENDER ./t.html TO /a.html\nIMPORT d FROM ./d\nEXPORT /', 'IMPORT r FROM https://e.x/a.json AS json\nRENDER <<H TO /i.html\n${r.x}\nH\nEXPORT /',
  'RENDER --escape=none <<H TO /i.html\nx\nH\nRENDER <<H TO /p/${x}.HTM\nx\nH\nEXPORT /', 'SITEMAP BASE https://e.x\nRENDER --escape=html <<H TO /a.html\nx\nH\nEXPORT /',
  'RENDER --escape=html <<H TO /a.html\nx\nH\nRENDER --escape=html --overwrite <<H TO /a.html\ny\nH\nEXPORT /',
  'FROM image ghcr.io/a/site:1\nEXPORT /', 'FROM image ghcr.io/a/site@sha256:' + 'a'.repeat(64) + '\nEXPORT /', 'FROM image site:1\nEXPORT / AS oci BASE nginx:alpine',
  '# sitefile-lint: disable=all\nARG A\nEXPORT /', '# sitefile-lint: disable-next-line=unused-name\nARG A\nARG B\nEXPORT /', '# sitefile-lint: disable=bogus,unused-name\n# sitefile-lint: nope\nARG A\nEXPORT /',
  'RENDER --escape=js t TO /x\nEXPORT /', 'RENDER t /x\nEXPORT /', 'RENDER t TO /x FOR a\nEXPORT /', 'IMPORT x FROM ./x AS json AS json\nEXPORT /',
  // Git imports (issue #91): refs, commit pins, paths, and what the planner refuses.
  'IMPORT d FROM git+https://e.x/r.git#main:docs AS tree\nCOPY d: /\nEXPORT /', 'IMPORT d FROM git+https://e.x/r.git\nEXPORT /',
  'IMPORT d FROM git+https://e.x/r#' + 'ab'.repeat(20) + ':a/b/ AS markdown SORT BY date\nEXPORT /', 'IMPORT d FROM git+http://e.x/r#v1.0:README.md AS text\nEXPORT /',
  'IMPORT d FROM git+https://e.x/r#refs/tags/v1\nEXPORT /', 'IMPORT d FROM GIT+HTTPS://e.x/r#:sub\nEXPORT /', 'IMPORT d FROM git+ssh://e.x/r\nEXPORT /',
  'IMPORT d FROM git+https://u:p@e.x/r\nEXPORT /', 'IMPORT d FROM git+https://e.x/r?x=1\nEXPORT /', 'IMPORT d FROM git+https://e.x/r#a..b\nEXPORT /',
  'IMPORT d FROM git+https://e.x/r#main:../x\nEXPORT /', 'IMPORT d FROM git+https://e.x/r#-x\nEXPORT /', 'IMPORT d FROM git+https://e.x/r#a b\nEXPORT /',
  'IMPORT d FROM git+https://e.x/r#main INTEGRITY sha256:' + 'a'.repeat(64) + '\nEXPORT /', 'IMPORT d FROM git+https://e.x/r#x.lock\nEXPORT /',
  'IMPORT d FROM "git+https://e.x/r#main:a\\\\b"\nEXPORT /', 'ARG R = main\nIMPORT d FROM git+https://e.x/r#${R}:docs\nEXPORT /', 'IMPORT d FROM git+:x\nEXPORT /',
  // Build secrets (issue #85).
  'IMPORT d FROM https://e.x/d SECRET k AS HEADER X-Api-Key AS json SECRET t AS BEARER\nEXPORT /', 'IMPORT d FROM ./d SECRET k AS BEARER\nEXPORT /',
  'IMPORT d FROM https://e.x/d SECRET k AS HEADER Content-Length\nEXPORT /', 'IMPORT d FROM https://e.x/d SECRET k AS HEADER xé\nEXPORT /',
  'IMPORT d FROM https://e.x/d SECRET a AS BEARER SECRET b AS HEADER AUTHORIZATION\nEXPORT /', 'IMPORT d FROM git+https://e.x/r SECRET k as header Cookie\nEXPORT /',
  'IMPORT d FROM https://e.x/d SECRET k AS COOKIE\nEXPORT /', 'IMPORT d FROM https://e.x/d SECRET\nEXPORT /', 'IMPORT d FROM https://e.x/d SECRET k AS HEADER "X-Q"\nEXPORT /',
  'FROM image\nEXPORT /', 'FROM image AS x\nEXPORT /', 'FROM image t:1 AT\nEXPORT /', 'FROM image t:1 AS\nEXPORT /', 'FROM image t:1 extra\nEXPORT /',
  'STAGE s FROM image ghcr.io/a/b:1 AT /srv/www\nEXPORT /', 'STAGE s FROM image\nEXPORT /', 'STAGE s FROM image t:1 AS x\nEXPORT /', 'FROM image Bad:1\nEXPORT /',
  'ARG V=1\nFROM image theme:${V} AS t\nFROM t\nEXPORT /', 'FROM image t:1 AT rel\nEXPORT /', 'FROM image t@sha256:' + 'a'.repeat(64) + ' AT /x/../y\nEXPORT /',
  'STAGE image\nFROM image\nEXPORT /', 'STAGE image\nFROM image AS b\nEXPORT /', 'FROM image ${NOPE}\nEXPORT /',
  // html fragments (issue #106).
  'LET a: html = <b>x</b>\nEXPORT /', 'LET a: html <<H\n<i>\nH\nEXPORT /', 'LET a: html = <<-H\n\t<p>\n\tH\nEXPORT /', 'LET a: html\nEXPORT /',
  'LET a: html <b>\nEXPORT /', 'ARG a: html\nEXPORT /', 'LET a: html = <i> \\\n <b>\nEXPORT /', 'LET a: html = <b> <<X\nfoo\nX\nEXPORT /',
  'LET a: html <<X\nfoo\nEXPORT /', 'LET a: html = ${a}\nEXPORT /', 'LET a: html = ${B}\nLET B = 1\nEXPORT /', 'ARG T\nLET a: html = ${T.x}\nEXPORT /',
  'LET a: html = x\nLET b: html = ${#each a as i}${/each}\nEXPORT /', 'STAGE s\nLET a: html = ${s}\nEXPORT /', 'LET a: html = ${#each}\nEXPORT /',
  'LET a: html <<H\nok\n  ${x y}\nH\nEXPORT /', 'LET a: html = x\nCOPY ./a /${a}\nEXPORT /', 'IMPORT h FROM ./h.html AS html\nEXPORT /${h}',
  'IMPORT h FROM ./h.html AS html\nRENDER ./t TO /x FOR i IN h\nEXPORT /', 'LET h: html = x\nFEED h BASE https://e.x LINK /a\nEXPORT /',
  'IMPORT h FROM ./h.html AS html SORT BY x\nEXPORT /', 'LET a = <<EOF\nEXPORT /', 'LET a: htmlx = 1\nEXPORT /', 'LET a:HTML=<i>\nEXPORT /',
  'ARG T = x\nLET a: html = <b>${T}</b>\nLET b: html = ${a}${item.title}\nRENDER --escape=html <<H TO /i.html\n${b}\nH\nEXPORT /',
  'LET a: html = x\nLET a = 1\nEXPORT /', 'RENDER --inline-svg --escape=html <<H TO /a.html\n${file /a.svg | svg}\nH\nEXPORT /',
  'LET a: html = ${file "/i/${x}.png" | img alt="${y.z}"}\nEXPORT /', 'ARG T\nLET a: html = ${file "/i/${T.x}.png"}\nEXPORT /', 'LET f: html = x\nLET a: html = ${file "/${f}"}\nEXPORT /', 'LET a: html = x\nRENDER ./t TO /${a}.html\nEXPORT /', 'IMPORT h FROM ./h AS html\nCOPY h: /h\nEXPORT /',
];

// ---------------------------------------------------------------- write

const fuzzOut = process.env.FUZZ_OUT;
const count = Number(process.env.FUZZ_COUNT ?? 400);
const NOISE = Number(process.env.FUZZ_NOISE ?? 0.08);
const cases = [];
const seen = new Set();
const add = (src) => {
  if (seen.has(src)) return;
  seen.add(src);
  cases.push(record(lib, src));
};
if (!fuzzOut) EDGE.forEach(add);
for (let n = 0; cases.length < (fuzzOut ? 0 : EDGE.length) + count && n < count * 3; n++) add(fuzzCase());
const crashes = cases.filter((c) => c.parse.crash || c.plan?.crash);
writeFileSync(fuzzOut ?? path.join(here, 'fuzz.json'), '[\n' + cases.map((c) => JSON.stringify(c)).join(',\n') + '\n]\n');
const ok = cases.filter((c) => c.parse.ok).length;
console.log(`${cases.length} inputs (${ok} parse, ${cases.filter((c) => c.plan?.ok).length} plan)${crashes.length ? `, ${crashes.length} TypeScript crashes` : ''}`);

if (!fuzzOut) {
  // `sitefile validate` (TypeScript CLI) on every Sitefile in the repository.
  const home = mkdtempSync(path.join(os.tmpdir(), 'sitefile-fuzz-home-'));
  const dirs = [];
  for (const base of ['conformance/cases', 'examples']) {
    for (const d of readdirSync(path.join(root, base)).sort()) {
      if (statSync(path.join(root, base, d, 'Sitefile'), { throwIfNoEntry: false })?.isFile()) dirs.push(`${base}/${d}`);
    }
  }
  const out = [];
  for (const dir of dirs) {
    const src = readFileSync(path.join(root, dir, 'Sitefile'), 'utf8');
    const args = [];
    const r = record(lib, src);
    for (const ins of r.parse.ok ?? []) if (ins.kind === 'ARG' && !ins.defaultValue) args.push('--build-arg', `${ins.name}=${placeholder(ins.type)}`);
    const r2 = spawnSync(process.execPath, [path.join(root, 'lib/src/cli.js'), 'validate', ...args], {
      cwd: path.join(root, dir),
      env: { ...process.env, SITEFILE_HOME: home, NO_COLOR: '1' },
      encoding: 'utf8',
    });
    const { status: code, stdout, stderr } = r2;
    out.push({ dir, args, code, stdout, stderr });
  }
  rmSync(home, { recursive: true, force: true });
  writeFileSync(path.join(here, 'conformance.json'), '[\n' + out.map((c) => JSON.stringify(c)).join(',\n') + '\n]\n');
  console.log(`${out.length} Sitefiles validated with the TypeScript CLI`);
}

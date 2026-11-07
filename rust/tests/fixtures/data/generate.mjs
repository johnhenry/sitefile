// Regenerates the YAML / CSV / frontmatter / template differential fixtures
// from the TypeScript implementation.
//
//   npm ci && npm run build        # at the repository root (builds lib/)
//   node rust/tests/fixtures/data/generate.mjs
//
// Each fixture is {input, ok: <JSON text of the value>} or
// {input, error: {code, detail, line, column}}. Checked by the Rust tests
// `yaml::tests::differential_fixtures`, `csv::tests::differential_fixtures`,
// `frontmatter::tests::differential_fixtures` and
// `render::tests::differential_fixtures`.
import { writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '../../../..');
const lib = (m) => import(path.join(root, 'lib/src', m));
const { parseYaml } = await lib('yaml.js');
const { parseCsv } = await lib('csv.js');
const { parseFrontmatter } = await lib('frontmatter.js');
const { parseRenderTemplate, renderTemplate, resolveJsonPath, walk } = await lib('render.js');
const { SitefileError } = await lib('errors.js');

let seed = Number(process.env.FUZZ_SEED ?? 0x5eed);
const rand = () => {
  seed = (seed + 0x6d2b79f5) | 0;
  let t = seed;
  t = Math.imul(t ^ (t >>> 15), t | 1);
  t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
};
const pick = (a) => a[Math.floor(rand() * a.length)];
const count = Number(process.env.FUZZ_COUNT ?? 1500);

function run(input, fn) {
  try {
    return { input, ok: JSON.stringify(fn(input)) };
  } catch (e) {
    if (!(e instanceof SitefileError)) { if (process.env.DEBUG) console.error(e); return null; } // a TS crash (TypeError) is not a contract
    return { input, error: { code: e.code, detail: e.detail, line: e.span?.line ?? 0, column: e.span?.column ?? 0 } };
  }
}

function collect(gen, fn) {
  const out = [];
  for (let n = 0; n < count; n++) {
    const r = run(gen(), fn);
    if (r) out.push(r);
  }
  return out;
}

// ------------------------------------------------------------------ YAML
const yKeys = ['a', 'b', 'key', '"q k"', "'s'", '1', '10', '2', '__proto__', '<<', '&x a', '? a', '"a: b"', 'k k', 'é', '😀'];
const yVals = [
  '', ' 1', ' -2', ' 1.5', ' 1.0', ' 007', ' -0', ' 12345678901234567890', ' true', ' false', ' null', ' ~', ' x', ' x # c', ' "s"', " 'it''s'",
  ' "a\\nb"', ' "bad \\q"', ' "open', ' [1, 2]', ' [a, [b, {c: d}]]', ' {x: 1, y: [2]}', ' {x}', ' [1,,2]', ' |', ' >-', ' |+', ' |2', ' &a 1',
  ' *a', ' !t x', ' b: c', ' - x', ' http://x.com:80/#f', ' 2026-01-02', ' [1] x', ' "x" y', '\t1', ' {a: 1, a: 2}', ' [a: 1]', ' 1e5', ' .5',
];
const yGood = [
  '', ' 1', ' -2', ' 1.5', ' 1.0', ' 007', ' -0', ' 12345678901234567890', ' true', ' false', ' null', ' ~', ' x', ' x # c', ' "s"', " 'it''s'",
  ' "a\\nb"', ' [1, 2]', ' [a, [b, {c: d}]]', ' {x: 1, y: [2]}', ' |', ' >-', ' |+', ' >', ' http://x.com:80/#f', ' 2026-01-02', ' 1e5', ' .5',
  ' a b c', ' 😀 é', ' {"q": [1, {z: ~}]}',
];
const yGoodKeys = ['a', 'b', 'key', '"q k"', "'s'", '1', '10', '2', '__proto__', 'k k', 'é', '😀', 'c', 'd'];
const yLine = () => {
  const clean = rand() < 0.9;
  const ind = clean ? pick(['', '', '', '  ', '  ', '    ']) : pick(['', ' ', '\t', '   ']);
  const val = () => (clean ? pick(yGood) : pick(yVals));
  const r = rand();
  if (r < 0.2) return ind + '-' + val();
  if (r < 0.25) return ind + (clean ? pick(['# c', '']) : pick(['---', '...', '--- x', '  text', 'plain']));
  if (r < 0.32) return ind + pick(['  line', '  more', '    deep', '']);
  return ind + (clean ? pick(yGoodKeys) : pick(yKeys)) + ':' + val();
};
// Structurally valid documents: nested block maps and sequences with random leaves.
const leaf = () => pick(yGood.filter((v) => v !== ' |' && v !== ' >-' && v !== ' |+' && v !== ' >'));
const yTree = (ind, depth) => {
  const lines = [];
  const n = 1 + Math.floor(rand() * 3);
  const seq = rand() < 0.4;
  const keys = [...yGoodKeys].sort(() => rand() - 0.5);
  for (let k = 0; k < n; k++) {
    const head = seq ? ind + '-' : ind + keys[k] + ':';
    const r = rand();
    if (depth < 4 && r < 0.3) {
      lines.push(head);
      lines.push(...yTree(ind + '  ', depth + 1));
    } else if (r < 0.4) {
      lines.push(head + pick([' |', ' >', ' |-', ' >+']));
      lines.push(...Array.from({ length: 1 + Math.floor(rand() * 3) }, () => ind + '  ' + pick(['text', 'more words', '  indented', '', '# x', 'k: v'])));
    } else lines.push(head + leaf());
  }
  return lines;
};
const yamlTree = collect(() => yTree('', 0).join('\n') + '\n', (t) => parseYaml(t, 'f.yml'));
const yaml = collect(() => Array.from({ length: 1 + Math.floor(rand() * 7) }, yLine).join(pick(['\n', '\n', '\r\n'])) + pick(['', '\n']), (t) => parseYaml(t, 'f.yml'));
const fm = collect(
  () => pick(['---\n', '---\r\n', '﻿---\n', '', '--- x\n']) + Array.from({ length: Math.floor(rand() * 5) }, yLine).join('\n') + pick(['\n---\nbody', '\n...\n', '\n---', '']),
  (t) => {
    const r = parseFrontmatter(t, 'p.md');
    return [r.data, r.body, r.bodyLine];
  },
);

// ------------------------------------------------------------------- CSV
const cAtoms = ['a', 'b', '1', ',', ',', '"', '""', '"x"', '\n', '\r\n', ' ', 'é', '😀', ';', '\t', '__proto__', '2'];
const cGood = ['a', 'b', '1', '"x,y"', '"q""q"', '"l\nl"', ' ', 'é', '😀', ';', '\t', '__proto__', '2', 'ccc'];
const csvGen = () => {
  if (rand() < 0.5) {
    // a grid: header plus rows with the same field count (sometimes one off)
    const cols = 1 + Math.floor(rand() * 4);
    const cell = () => Array.from({ length: Math.floor(rand() * 3) }, () => pick(cGood)).join('');
    const head = Array.from({ length: cols }, (_, i) => cell() || `h${i}`).join(',');
    const rows = Array.from({ length: Math.floor(rand() * 4) }, () => Array.from({ length: rand() < 0.9 ? cols : cols + 1 }, cell).join(','));
    return [head, ...rows].join(pick(['\n', '\r\n', '\n\n'])) + pick(['', '\n', '\r\n']);
  }
  return Array.from({ length: 1 + Math.floor(rand() * 16) }, () => pick(cAtoms)).join('');
};
const csv = collect(csvGen, (t) => parseCsv(t, 'f.csv'));
const csvRaw = collect(csvGen, (t) => parseCsv(t, 'f.csv', { header: false }));

// -------------------------------------------------------------- templates
const data = {
  T: 'A & <b>', N: 3, F: 1.5, Z: 0, B: false, E: '', nil: null,
  list: [{ t: 'x', tags: ['a', 'b'] }, { t: 'y', tags: [] }, { t: 'z' }], empty: [], obj: { k: 'v', n: [1, 2] },
};
const lookup = (name, p, span) => {
  if (!(name in data)) throw new SitefileError('E_UNDEFINED', `'${name}' is not an ARG, LET or IMPORT`, span);
  return resolveJsonPath(data[name], p, name, span);
};
const values = (name, p, span, loose) => {
  if (!(name in data)) {
    if (loose) return undefined;
    throw new SitefileError('E_UNDEFINED', `'${name}' is not an ARG, LET or IMPORT`, span);
  }
  return walk(data[name], p, name, span, loose);
};
// html fragments (issue #106): parsed from these templates, rendered in place.
const FRAGMENTS = { F1: '<b title="${T}">${T}</b>', F2: '[${F1}|${i.t}]', LOOP: '${#each list as x}${x.t}${/each}', REC: '(${REC2})', REC2: '${REC}', BAD: 'x ${#if}' };
// File helpers (issue #105): a small tree (a 3x2 PNG header, a stylesheet, a directory).
const PNG = Uint8Array.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13, 0x49, 0x48, 0x44, 0x52, 0, 0, 0, 3, 0, 0, 0, 2]);
const META = JSON.stringify({ version: 1, src: '/m-2.png', type: 'image/png', width: 2, height: 1, srcset: '/m-1.png 1w, /m-2.png 2w', sources: [{ type: 'image/webp', srcset: '/m-1.webp 1w' }, { type: 'image/png', srcset: '/m-1.png 1w, /m-2.png 2w' }], variants: [] });
const FILES = new Map([['/m.json', new TextEncoder().encode(META)], ['/a.png', PNG], ['/s.css', new TextEncoder().encode('h1 { color: <red> }\n')], ['/d/x.txt', new TextEncoder().encode('x')]]);
const ctx = {
  isFragment: (name) => Object.hasOwn(FRAGMENTS, name),
  fragment: (name) => parseRenderTemplate(FRAGMENTS[name], `${name}.html`, 1),
  tree: { get: (p) => FILES.get(p), isDirectory: (p) => [...FILES.keys()].some((k) => k.startsWith(p + '/')) },
  inlineSvg: false,
};
const tAtoms = [
  '${file "/a.png" | img alt="${T}"}', '${file /s.css | stylesheet}', '${file "/nope"}', '${file "/d"}', '${file a.png}', '${file "/a.png" | width}',
  '${file "/s.css" | pre class=x}', '${file "/a.png" | bogus}', '${file }', '${file "/a.png" | img alt}', '${file "/a.png" | img alt=1 ALT=2}',
  "${file '/a${x}.png'}", '${file "/a${i.t}.png"}', '${file "/s.css" | svg}', '${file "x}', '${file \'/s.css\' | datauri}', '${file "/a.png" | url x}',
  '${file "/a.png"|img loading=eager data-x="a}b"}', '${file "/m.png" | img alt="${T}" sizes=50vw}', '${file "/m.json" | srcset}', '${file /m.png | width}', '${file /m.png}', '${file /a.png | srcset}', '${file /s.css | img}', '${file "/a.png" | img "x"}', '${file "/a.png" x}', '${file "/s.css" | width}', '${file "/a.png" | integrity}',
  '${F1}', '${F2}', '${LOOP}', '${REC}', '${BAD}', '${F1|raw}', '${F1.x}', '${#each F1 as i}', '${#if F2}', '${#each list as F1}${F1}${/each}',
  '${T}', '${N}', '${F}', '${Z}', '${B}', '${E}', '${T|raw}', '${nil}', '${obj}', '${obj.k}', '${obj.n.1}', '${obj.n.5}', '${list.0.t}', '${nope}',
  '${#each list as i}', '${#each obj.n as i}', '${#each list.0.tags as T}', '${#each nope as i}', '${#each T as i}', '${#each missing.k as i}',
  '${i}', '${i.t}', '${i.tags.0}', '${#if i.tags}', '${#if !i}', '${#if B}', '${#if Z}', '${#if empty}', '${#if obj}', '${#if nope.x}', '${else}',
  '${/each}', '${/if}', '$${', '$', '${', '${ a }', '${a b}', '${#each x}', '${#if}', '${/each x}', '${else x}', 'text', ' ', '\n', '\r\n', '  ',
  '<p>', '&', '"', "'", 'é', '😀', '}', '${a.}', '${a..b}', '${1x}', '${x|upper}',
];
const tGood = [
  '${file "/m.png" | img alt="${T}" sizes=50vw}', '${file /m.png | width}', '${file "/a.png" | img alt="${T}"}', '${F1}', '${file /s.css | stylesheet}',
  '${T}', '${N}', '${F}', '${Z}', '${B}', '${E}', '${T|raw}', '${obj.k}', '${obj.n.1}', '${list.0.t}', 'text', ' ', '\n', '\r\n', '  ',
  '<p>', '&', '"', "'", 'é', '😀', '}', '$${', '$', '$5',
];
const inner = () => Array.from({ length: Math.floor(rand() * 4) }, () => pick(tGood)).join('');
const nl = () => pick(['', '', '\n', '\n  ', '  \n', '\r\n']);
const conds = ['T', 'Z', 'B', 'E', 'N', 'nil', 'empty', 'obj', 'list', 'nope.x', 'list.0.tags', 'list.1.tags', 'obj.k'];
const tBlock = () => {
  const r = rand();
  if (r < 0.25) return nl() + '${#each list as i}' + nl() + '[${i.t}' + inner() + '${#each i.tags as t}${t},${else}-${/each}]' + nl() + '${/each}' + nl();
  if (r < 0.4) return '${#each obj.n as T}${T}' + inner() + '${/each}';
  if (r < 0.55) return '${#each empty as x}x${else}' + inner() + '${/each}';
  if (r < 0.75) return nl() + '${#if ' + pick(['!', '', '! ']) + pick(conds) + '}' + nl() + inner() + pick(['', '${else}' + inner()]) + '${/if}' + nl();
  return inner();
};
const tpl = collect(
  () =>
    rand() < 0.6
      ? Array.from({ length: 1 + Math.floor(rand() * 4) }, tBlock).join('')
      : Array.from({ length: 1 + Math.floor(rand() * 12) }, () => pick(tAtoms)).join(''),
  (t) => {
    const nodes = parseRenderTemplate(t, 't.html', 3);
    return [renderTemplate(nodes, lookup, 'html', values, ctx), renderTemplate(nodes, lookup, 'none', values, ctx)];
  },
);

const write = (name, v) => writeFileSync(path.join(process.env.FUZZ_DIR ?? here, name), JSON.stringify(v) + '\n');
write('yaml.json', [...yaml, ...yamlTree]);
write('frontmatter.json', fm);
write('csv.json', csv);
write('csv-raw.json', csvRaw);
write('template.json', tpl);
console.log(`yaml ${yaml.length}, frontmatter ${fm.length}, csv ${csv.length + csvRaw.length}, template ${tpl.length}`);

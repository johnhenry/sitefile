#!/usr/bin/env node
// Deterministic synthetic inputs for the benchmarks (no randomness, no clock).
//
//   node bench/generate.mjs <outDir>
//
// Creates:
//   blog-100, blog-1000, blog-5000   Markdown blogs (frontmatter, lists, code, links)
//   static-5000                      5000 static files (~10 KB each) copied as-is
//   csv-2000                         a 2000-row CSV rendered to one page per row
//   archive-2000                     a tar.gz of 2000 files imported AS tree

import { promises as fs } from 'node:fs';
import path from 'node:path';
import { gzipSync } from 'node:zlib';

const out = path.resolve(process.argv[2] ?? 'bench/.sites');

// Small deterministic PRNG (xorshift32) so content varies but is reproducible.
function rng(seed) {
  let x = seed >>> 0 || 1;
  return () => ((x ^= x << 13), (x ^= x >>> 17), (x ^= x << 5), (x >>> 0) / 4294967296);
}
const WORDS = 'site build static sitefile render template markdown stage export archive bundle image layer digest plugin sandbox deterministic content tree path asset style script feed post page index'.split(' ');
const words = (r, n) => Array.from({ length: n }, () => WORDS[Math.floor(r() * WORDS.length)]).join(' ');

async function write(file, data) {
  await fs.mkdir(path.dirname(file), { recursive: true });
  await fs.writeFile(file, data);
}

const POST_TEMPLATE = `<!doctype html><html><head><meta charset="utf-8"><title>\${post.title} · \${SITE}</title></head>
<body><h1>\${post.title}</h1><p class="meta">\${post.date}</p>
\${#if post.tags}<ul>\${#each post.tags as t}<li>\${t}</li>\${/each}</ul>\${/if}
\${post.content|raw}
<nav>\${#each posts as other}<a href="/posts/\${other.slug}/">\${other.title}</a>\${/each}</nav>
</body></html>
`;
const INDEX_TEMPLATE = `<!doctype html><html><body><h1>\${SITE}</h1>
\${#each posts as post}<article><h2><a href="/posts/\${post.slug}/">\${post.title}</a></h2><p>\${post.excerpt}</p></article>
\${/each}</body></html>
`;

async function blog(n) {
  const dir = path.join(out, `blog-${n}`);
  const r = rng(n);
  for (let i = 0; i < n; i++) {
    const day = String(1 + (i % 28)).padStart(2, '0');
    const month = String(1 + (Math.floor(i / 28) % 12)).padStart(2, '0');
    const year = 2000 + Math.floor(i / 336);
    const body = [
      `# ${words(r, 4)}`,
      '',
      `${words(r, 40)} *${words(r, 3)}* and **${words(r, 2)}** with \`${words(r, 1)}\` and [a link](https://example.com/${i}).`,
      '',
      `- ${words(r, 6)}`,
      `- ${words(r, 6)}`,
      `  - ${words(r, 4)}`,
      '',
      '```js',
      `const x = ${i}; // ${words(r, 3)}`,
      '```',
      '',
      `> ${words(r, 12)}`,
      '',
      words(r, 80),
    ].join('\n');
    await write(
      path.join(dir, 'posts', `${year}-${month}-${day}-post-${i}.md`),
      `---\ntitle: "Post ${i}: ${words(r, 3)}"\ntags: [${words(r, 2).replace(' ', ', ')}]\n---\n${body}\n`,
    );
  }
  // FOR over all posts with an "all posts" nav is O(n^2) output; keep the nav for n ≤ 1000.
  const post = n <= 1000 ? POST_TEMPLATE : POST_TEMPLATE.replace(/<nav>[\s\S]*<\/nav>\n/, '');
  await write(path.join(dir, 'templates', 'post.html'), post);
  await write(path.join(dir, 'templates', 'index.html'), INDEX_TEMPLATE);
  await write(
    path.join(dir, 'Sitefile'),
    `FROM scratch\nARG SITE = "Bench"\nIMPORT posts FROM ./posts AS markdown SORT BY date DESC\nRENDER --escape=html ./templates/index.html TO /index.html\nRENDER --escape=html ./templates/post.html TO /posts/\${post.slug}/index.html FOR post IN posts\nEXPORT /\n`,
  );
}

async function staticFiles(n) {
  const dir = path.join(out, `static-${n}`);
  const r = rng(7);
  for (let i = 0; i < n; i++) {
    const sub = `d${Math.floor(i / 100)}`;
    await write(path.join(dir, 'public', sub, `f${i}.txt`), (words(r, 30) + '\n').repeat(50));
  }
  await write(path.join(dir, 'Sitefile'), 'FROM scratch\nCOPY ./public/ /\nEXPORT /\n');
}

async function csvPages(n) {
  const dir = path.join(out, `csv-${n}`);
  const r = rng(11);
  const rows = ['slug,name,score,note'];
  for (let i = 0; i < n; i++) rows.push(`p${i},"Person ${i}, ${words(r, 1)}",${Math.floor(r() * 1000)},"${words(r, 8)}"`);
  await write(path.join(dir, 'people.csv'), rows.join('\n') + '\n');
  await write(
    path.join(dir, 'Sitefile'),
    `FROM scratch\nIMPORT people FROM ./people.csv AS csv SORT BY score DESC\nRENDER --escape=html <<T TO /people/\${p.slug}.html FOR p IN people\n<h1>\${p.name}</h1><p>\${p.score}</p><p>\${p.note}</p>\nT\nRENDER --escape=html <<T TO /index.html\n<ul>\${#each people as p}<li>\${p.name}: \${p.score}</li>\${/each}</ul>\nT\nEXPORT /\n`,
  );
}

function tarOf(files) {
  const parts = [];
  for (const [name, data] of files) {
    const h = Buffer.alloc(512);
    h.write(name, 0, 100);
    h.write('0000644\0', 100);
    h.write('0000000\0', 108);
    h.write('0000000\0', 116);
    h.write(data.length.toString(8).padStart(11, '0') + '\0', 124);
    h.write('00000000000\0', 136);
    h.write('        ', 148);
    h.write('0', 156);
    h.write('ustar\u000000', 257);
    let sum = 0;
    for (const b of h) sum += b;
    h.write(sum.toString(8).padStart(6, '0') + '\0 ', 148);
    parts.push(h, data, Buffer.alloc((512 - (data.length % 512)) % 512));
  }
  parts.push(Buffer.alloc(1024));
  return Buffer.concat(parts);
}

async function archive(n) {
  const dir = path.join(out, `archive-${n}`);
  const r = rng(13);
  const files = Array.from({ length: n }, (_, i) => [`lib/m${Math.floor(i / 50)}/f${i}.js`, Buffer.from(`export const v${i} = ${JSON.stringify(words(r, 20))};\n`)]);
  await write(path.join(dir, 'vendor.tar.gz'), gzipSync(tarOf(files), { level: 6 }));
  await write(path.join(dir, 'Sitefile'), 'FROM scratch\nIMPORT vendor FROM ./vendor.tar.gz AS tree\nCOPY vendor:/lib/ /assets/\nEXPORT /\n');
}

await fs.rm(out, { recursive: true, force: true });
await Promise.all([blog(100), blog(1000), blog(5000), staticFiles(5000), csvPages(2000), archive(2000)]);
console.log(`wrote benchmark sites to ${out}`);

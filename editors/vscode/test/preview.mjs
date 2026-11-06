// Renders highlighted Sitefiles to test/preview.html (not committed) with shiki,
// using the real embedded HTML/CSS/JS/... grammars. `npm run preview`.
import { readFileSync, writeFileSync } from 'node:fs';
import { createHighlighter } from 'shiki';

const here = (p) => new URL(p, import.meta.url);
const grammar = JSON.parse(readFileSync(here('../syntaxes/sitefile.tmLanguage.json'), 'utf8'));
const injection = JSON.parse(readFileSync(here('../syntaxes/sitefile-template.injection.tmLanguage.json'), 'utf8'));
const embedded = ['html', 'css', 'javascript', 'markdown', 'json', 'yaml', 'xml'];

const highlighter = await createHighlighter({
  themes: ['light-plus', 'dark-plus'],
  langs: [
    ...embedded,
    { ...grammar, name: 'sitefile', embeddedLangs: embedded },
    { ...injection, name: 'sitefile-template-injection', injectTo: ['source.sitefile'] },
  ],
});

const files = [
  ['examples/blog/Sitefile', '../../../examples/blog/Sitefile'],
  ['examples/inline/Sitefile', '../../../examples/inline/Sitefile'],
  ['examples/motion/Sitefile', '../../../examples/motion/Sitefile'],
  ['test/fixtures/edge-cases.Sitefile', './fixtures/edge-cases.Sitefile'],
  ['test/fixtures/invalid-errors.Sitefile', './fixtures/invalid-errors.Sitefile'],
];
const sections = files
  .map(([label, p]) => {
    const html = highlighter.codeToHtml(readFileSync(here(p), 'utf8'), {
      lang: 'sitefile',
      themes: { light: 'light-plus', dark: 'dark-plus' },
    });
    return `<h2>${label}</h2>\n${html}`;
  })
  .join('\n');

const out = here('./preview.html');
writeFileSync(
  out,
  `<!doctype html>
<meta charset="utf-8">
<title>Sitefile grammar preview</title>
<style>
  body { font-family: system-ui, sans-serif; max-width: 70rem; margin: 2rem auto; padding: 0 1rem; }
  pre { padding: 1rem; border-radius: 6px; overflow-x: auto; font-size: 13px; line-height: 1.45; }
  @media (prefers-color-scheme: dark) {
    body { background: #0d1117; color: #e6edf3; }
    .shiki, .shiki span { color: var(--shiki-dark) !important; background-color: var(--shiki-dark-bg) !important; }
  }
</style>
<h1>Sitefile grammar preview</h1>
${sections}
`,
);
console.log(out.pathname);

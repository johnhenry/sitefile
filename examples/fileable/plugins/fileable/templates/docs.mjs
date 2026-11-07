// A fileable template: a small docs section, one page per entry of the
// value import, an index, and a JSON search index. Written with fileable's
// builders (h, dir, file); compiled JSX produces the same descriptors.
import { h, dir, file, markdownToHtml } from '@johnhenry/fileable/browser';

const page = (title, body, css) => [
  '<!doctype html>\n',
  h('html', { lang: 'en' },
    h('head', null, h('meta', { charset: 'utf-8' }), h('title', null, title), h('link', { rel: 'stylesheet', href: css })),
    h('body', null, h('main', null, body))),
  '\n',
];

export default function docs({ data, options }) {
  const site = options.title ?? 'Docs';
  const pages = [...data.pages].sort((a, b) => a.order - b.order);
  return [
    file('index.html', page(site, [
      h('h1', null, site),
      h('ul', null, pages.map((p) => h('li', null, h('a', { href: `pages/${p.slug}.html` }, p.title)))),
    ], 'style.css'), { doctype: 'html' }),
    dir('pages', pages.map((p) =>
      file(`${p.slug}.html`, page(`${p.title} - ${site}`, [
        h('h1', null, p.title),
        // markdownToHtml (fileable's marked wrapper) returns HTML, which fileable inserts as markup.
        markdownToHtml(p.body),
        h('p', null, h('a', { href: '../index.html' }, 'All pages')),
      ], '../style.css'), { doctype: 'html' }))),
    file('search.json', JSON.stringify(pages.map(({ slug, title }) => ({ slug, title })), null, 2) + '\n'),
  ];
}

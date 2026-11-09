// LANGUAGES (docs/LANGUAGE.md, "Site files"): which pages translate each other, for the hreflang alternates
// CANONICAL and SITEMAP write. Ported as rust/src/hreflang.rs: both must find the same groups and messages.
import { SitefileError, type SourceSpan } from './errors.js';

const TAG_RE = /^[A-Za-z]{2,3}(-[A-Za-z0-9]{1,8})*$/;

/** A basic BCP 47 check: a 2-3 letter language, then `-` subtags of 1-8 letters or digits (`en`, `pt-BR`, `zh-Hant-TW`). */
export function isLanguageTag(s: string): boolean {
  return TAG_RE.test(s);
}

/** What a document's frontmatter says about the page it renders: `lang` and `translationKey`. */
export interface Translation {
  readonly lang: string;
  readonly key: string;
}

/** One alternate of a page: its `hreflang` value and the URL path of the translation (the caller makes it absolute). */
export type Alternate = readonly [hreflang: string, url: string];

/** LANGUAGES, evaluated, with the translations documents give (tree path -> translation). */
export interface Languages {
  readonly codes: readonly string[];
  readonly default?: string;
  readonly docs: ReadonlyMap<string, Translation>;
}

function utf8Compare(a: string, b: string): number {
  return Buffer.compare(Buffer.from(a, 'utf8'), Buffer.from(b, 'utf8'));
}

/**
 * The alternates of every page that has a translation. `pages` are [tree path, URL path] pairs (the URL path
 * is what the language folder is read from). A page a document renders is grouped by its `translationKey`
 * in its `lang`; any other page by its URL path without the language folder (`/fr/about.html` is the `fr`
 * page of `/about.html`), and one in no language folder is the DEFAULT language's (without DEFAULT, it has
 * none). A group of two or more pages gives each of them every page of the group, ordered by language, then
 * `x-default` for the DEFAULT language's page. Two pages of one language in a group are `E_VALIDATION`.
 */
export function alternates(pages: ReadonlyArray<readonly [string, string]>, langs: Languages, what: string, span: SourceSpan): Map<string, Alternate[]> {
  const groups = new Map<string, Array<{ path: string; url: string; lang: string }>>();
  for (const [path, url] of [...pages].sort((a, b) => utf8Compare(a[0], b[0]))) {
    const doc = langs.docs.get(path);
    let group: string;
    let lang: string;
    let label: string;
    if (doc) {
      group = `k\0${doc.key}`;
      lang = doc.lang;
      label = `translationKey '${doc.key}'`;
    } else {
      const slash = url.indexOf('/', 1);
      const folder = slash > 0 ? url.slice(1, slash) : undefined;
      if (folder !== undefined && langs.codes.includes(folder)) {
        lang = folder;
        group = `f\0${url.slice(slash)}`;
        label = `'${url.slice(slash)}'`;
      } else if (langs.default !== undefined) {
        lang = langs.default;
        group = `f\0${url}`;
        label = `'${url}'`;
      } else continue;
    }
    let members = groups.get(group);
    if (!members) groups.set(group, (members = []));
    const same = members.find((m) => m.lang.toLowerCase() === lang.toLowerCase());
    if (same) {
      throw new SitefileError('E_VALIDATION', `${what}: ${same.path} and ${path} are both the '${lang}' page of ${label}; a translation group has one page per language`, span);
    }
    members.push({ path, url, lang });
  }
  const out = new Map<string, Alternate[]>();
  const dflt = langs.default?.toLowerCase();
  for (const members of groups.values()) {
    if (members.length < 2) continue;
    const sorted = [...members].sort((a, b) => (a.lang.toLowerCase() < b.lang.toLowerCase() ? -1 : a.lang.toLowerCase() > b.lang.toLowerCase() ? 1 : 0));
    const alts: Alternate[] = sorted.map((m) => [m.lang, m.url]);
    const d = sorted.find((m) => m.lang.toLowerCase() === dflt);
    if (d) alts.push(['x-default', d.url]);
    for (const m of members) out.set(m.path, alts);
  }
  return out;
}

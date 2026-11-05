/**
 * A small, dependency-free, deterministic Markdown renderer (CommonMark-ish).
 *
 * Blocks: ATX headings (#..######, optional closing #s), setext headings,
 * paragraphs (hard break = two trailing spaces or a trailing backslash),
 * fenced code (``` and ~~~, info word becomes class="language-x" when it is
 * [A-Za-z0-9_+-]+), indented code, blockquotes (recursive, lazy continuation),
 * bullet lists (- * +) and ordered lists (1. / 1), `start` kept when not 1),
 * nesting by indentation, tight vs loose lists, thematic breaks (--- *** ___).
 *
 * Inlines: backslash escapes, code spans, **strong** / __strong__, *em* /
 * _em_ (intraword `_` is literal), ~~del~~, [text](url "title"),
 * ![alt](url "title"), <autolinks>. Emphasis that does not close stays literal.
 *
 * Not supported: reference links, HTML entities (`&` is always escaped, so
 * `&amp;` shows literally), tables, footnotes, task lists.
 *
 * Safety: raw HTML is never passed through; every `<` in the source is
 * escaped. Link and image URLs must be http, https, mailto, relative,
 * `#fragment` or `/absolute`; anything else (javascript:, data:, ...) is
 * rendered as plain text without an <a> / <img>.
 */

export interface MarkdownResult {
  html: string;
  /** Plain text of the first paragraph, at most 200 characters. */
  excerpt: string;
}

const MAX_DEPTH = 32;
const EXCERPT_MAX = 200;

// ---------------------------------------------------------------- escaping

export function escapeHtml(s: string): string {
  return s.replace(/[&<>"']/g, (c) => (c === '&' ? '&amp;' : c === '<' ? '&lt;' : c === '>' ? '&gt;' : c === '"' ? '&quot;' : '&#39;'));
}

/** Scheme check on a URL stripped of whitespace/control characters. */
function isSafeUrl(url: string): boolean {
  const stripped = url.replace(/[\u0000-\u0020\u007f-\u009f\u00ad\u200b-\u200f\u2028\u2029\ufeff]/g, '');
  const m = /^([^:/?#]*):/.exec(stripped);
  return m === null || /^(https?|mailto)$/i.test(m[1] ?? '');
}

function encodeUrl(url: string): string {
  return escapeHtml(url.replace(/[\u0000-\u001f\u007f]/g, '').replace(/ /g, '%20'));
}

const unescapeMd = (s: string): string => s.replace(/\\([!-/:-@[-`{-~])/g, '$1');

// ------------------------------------------------------------------ inlines

type Node =
  | { t: 'text'; v: string }
  | { t: 'code'; v: string }
  | { t: 'br' }
  | { t: 'wrap'; tag: 'em' | 'strong' | 'del'; c: Node[] }
  | { t: 'link'; href: string; title: string | null; c: Node[] }
  | { t: 'img'; src: string; alt: string; title: string | null };

interface Delim {
  t: 'delim';
  ch: string;
  n: number;
  orig: number;
  open: boolean;
  close: boolean;
}
type Item = Node | Delim;

const isPunct = (c: string | undefined): boolean => c !== undefined && /[\p{P}\p{S}]/u.test(c);
const isSpace = (c: string | undefined): boolean => c === undefined || /\s/.test(c);

/** Index of the `]` matching an already-opened `[` (scan starts at `from`), or -1. */
function findCloseBracket(s: string, from: number): number {
  let depth = 1;
  for (let j = from; j < s.length; j++) {
    const c = s[j];
    if (c === '\\') j++;
    else if (c === '`') {
      const run = /^`+/.exec(s.slice(j))?.[0] ?? '`';
      const end = s.indexOf(run, j + run.length);
      j = end === -1 ? j + run.length - 1 : end + run.length - 1;
    } else if (c === '[') depth++;
    else if (c === ']' && --depth === 0) return j;
  }
  return -1;
}

/** Parse `(dest "title")` starting at the `(` at s[k]. */
function parseDestination(s: string, k: number): { href: string; title: string | null; end: number } | null {
  const ws = (i: number): number => {
    while (i < s.length && /[ \t\n]/.test(s[i] as string)) i++;
    return i;
  };
  let i = ws(k + 1);
  let href: string;
  if (s[i] === '<') {
    let j = i + 1;
    while (j < s.length && s[j] !== '>' && s[j] !== '<' && s[j] !== '\n') j++;
    if (s[j] !== '>') return null;
    href = unescapeMd(s.slice(i + 1, j));
    i = j + 1;
  } else {
    const start = i;
    let depth = 0;
    while (i < s.length) {
      const c = s[i] as string;
      if (c === '\\' && i + 1 < s.length) {
        i += 2;
        continue;
      }
      if (/[\s\u0000-\u001f]/.test(c)) break;
      if (c === '(') depth++;
      else if (c === ')') {
        if (depth === 0) break;
        depth--;
      }
      i++;
    }
    if (depth !== 0) return null;
    href = unescapeMd(s.slice(start, i));
  }
  const afterDest = i;
  i = ws(i);
  if (s[i] === ')') return { href, title: null, end: i + 1 };
  const open = s[i];
  const close = open === '"' ? '"' : open === "'" ? "'" : open === '(' ? ')' : null;
  if (close === null || i === afterDest) return null;
  let j = i + 1;
  while (j < s.length && s[j] !== close) j += s[j] === '\\' ? 2 : 1;
  if (j >= s.length) return null;
  const title = unescapeMd(s.slice(i + 1, j));
  j = ws(j + 1);
  return s[j] === ')' ? { href, title, end: j + 1 } : null;
}

function parseInline(s: string, depth: number): Node[] {
  const items: Item[] = [];
  let buf = '';
  const flush = (): void => {
    if (buf !== '') items.push({ t: 'text', v: buf });
    buf = '';
  };

  let i = 0;
  while (i < s.length) {
    const c = s[i] as string;
    const next = s[i + 1];

    if (c === '\\') {
      if (next === '\n') {
        flush();
        items.push({ t: 'br' });
        i += 2;
      } else if (next !== undefined && /[!-/:-@[-`{-~]/.test(next)) {
        buf += next;
        i += 2;
      } else {
        buf += c;
        i++;
      }
    } else if (c === '\n') {
      const spaces = /( *)$/.exec(buf)?.[1]?.length ?? 0;
      buf = buf.slice(0, buf.length - spaces);
      if (spaces >= 2) {
        flush();
        items.push({ t: 'br' });
      } else {
        buf += '\n';
      }
      i++;
    } else if (c === '`') {
      const run = /^`+/.exec(s.slice(i))?.[0] ?? '`';
      let end = -1;
      for (let j = s.indexOf('`', i + run.length); j !== -1; ) {
        const r = /^`+/.exec(s.slice(j))?.[0] ?? '`';
        if (r.length === run.length) {
          end = j;
          break;
        }
        j = s.indexOf('`', j + r.length);
      }
      if (end === -1) {
        buf += run;
        i += run.length;
      } else {
        let code = s.slice(i + run.length, end).replace(/\n/g, ' ');
        if (code.length > 2 && code.startsWith(' ') && code.endsWith(' ') && code.trim() !== '') code = code.slice(1, -1);
        flush();
        items.push({ t: 'code', v: code });
        i = end + run.length;
      }
    } else if (c === '*' || c === '_' || c === '~') {
      let n = 1;
      while (s[i + n] === c) n++;
      if (c === '~' && n !== 2) {
        buf += c.repeat(n);
        i += n;
        continue;
      }
      const before = s[i - 1];
      const after = s[i + n];
      const left = !isSpace(after) && (!isPunct(after) || isSpace(before) || isPunct(before));
      const right = !isSpace(before) && (!isPunct(before) || isSpace(after) || isPunct(after));
      const open = c === '_' ? left && (!right || isPunct(before)) : left;
      const close = c === '_' ? right && (!left || isPunct(after)) : right;
      flush();
      items.push({ t: 'delim', ch: c, n, orig: n, open, close });
      i += n;
    } else if (c === '[' || (c === '!' && next === '[')) {
      const isImg = c === '!';
      const labelStart = i + (isImg ? 2 : 1);
      const closeIdx = depth < MAX_DEPTH ? findCloseBracket(s, labelStart) : -1;
      const dest = closeIdx !== -1 && s[closeIdx + 1] === '(' ? parseDestination(s, closeIdx + 1) : null;
      if (closeIdx === -1 || dest === null) {
        buf += isImg ? '![' : '[';
        i = labelStart;
        continue;
      }
      const children = parseInline(s.slice(labelStart, closeIdx), depth + 1);
      flush();
      if (isImg) {
        const alt = nodesToText(children);
        items.push(isSafeUrl(dest.href) ? { t: 'img', src: dest.href, alt, title: dest.title } : { t: 'text', v: alt });
      } else if (isSafeUrl(dest.href)) {
        items.push({ t: 'link', href: dest.href, title: dest.title, c: children });
      } else {
        items.push(...children);
      }
      i = dest.end;
    } else if (c === '<') {
      const auto = /^<([A-Za-z][A-Za-z0-9+.-]{1,31}:[^\s<>]*)>/.exec(s.slice(i));
      const mail = /^<([A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+)>/.exec(s.slice(i));
      if (auto && isSafeUrl(auto[1] as string)) {
        flush();
        items.push({ t: 'link', href: auto[1] as string, title: null, c: [{ t: 'text', v: auto[1] as string }] });
        i += auto[0].length;
      } else if (mail) {
        flush();
        items.push({ t: 'link', href: `mailto:${mail[1]}`, title: null, c: [{ t: 'text', v: mail[1] as string }] });
        i += mail[0].length;
      } else {
        buf += c;
        i++;
      }
    } else {
      buf += c;
      i++;
    }
  }
  flush();
  return resolveEmphasis(items);
}

const delimToText = (it: Item): Node => (it.t === 'delim' ? { t: 'text', v: it.ch.repeat(it.n) } : it);

/** CommonMark-style delimiter matching, simplified. */
function resolveEmphasis(items: Item[]): Node[] {
  for (let ci = 0; ci < items.length; ci++) {
    const closer = items[ci];
    if (closer?.t !== 'delim' || !closer.close) continue;
    while (closer.n > 0) {
      let oi = -1;
      for (let k = ci - 1; k >= 0; k--) {
        const o = items[k];
        if (o?.t !== 'delim' || o.ch !== closer.ch || !o.open || o.n === 0) continue;
        const rule3 = (o.close || closer.open) && (o.orig + closer.orig) % 3 === 0 && !(o.orig % 3 === 0 && closer.orig % 3 === 0);
        if (rule3) continue;
        oi = k;
        break;
      }
      if (oi === -1) break;
      const opener = items[oi] as Delim;
      const use = closer.ch === '~' ? 2 : opener.n >= 2 && closer.n >= 2 ? 2 : 1;
      const tag = closer.ch === '~' ? 'del' : use === 2 ? 'strong' : 'em';
      const inner = items.slice(oi + 1, ci).map(delimToText);
      opener.n -= use;
      closer.n -= use;
      items.splice(oi + 1, ci - oi - 1, { t: 'wrap', tag, c: inner });
      ci = oi + 2;
      if (opener.n === 0) {
        items.splice(oi, 1);
        ci--;
      }
      if (closer.n === 0) {
        items.splice(ci, 1);
        ci--;
        break;
      }
    }
  }
  return items.map(delimToText);
}

function nodesToText(nodes: Node[]): string {
  return nodes
    .map((n): string => {
      switch (n.t) {
        case 'text':
        case 'code':
          return n.v;
        case 'br':
          return ' ';
        case 'img':
          return n.alt;
        default:
          return nodesToText(n.c);
      }
    })
    .join('');
}

function nodesToHtml(nodes: Node[]): string {
  return nodes
    .map((n): string => {
      switch (n.t) {
        case 'text':
          return escapeHtml(n.v);
        case 'code':
          return `<code>${escapeHtml(n.v)}</code>`;
        case 'br':
          return '<br>\n';
        case 'wrap':
          return `<${n.tag}>${nodesToHtml(n.c)}</${n.tag}>`;
        case 'link':
          return `<a href="${encodeUrl(n.href)}"${n.title === null ? '' : ` title="${escapeHtml(n.title)}"`}>${nodesToHtml(n.c)}</a>`;
        case 'img':
          return `<img src="${encodeUrl(n.src)}" alt="${escapeHtml(n.alt)}"${n.title === null ? '' : ` title="${escapeHtml(n.title)}"`}>`;
      }
    })
    .join('');
}

// ------------------------------------------------------------------- blocks

interface Block {
  html: string;
  /** Inline HTML when this block is a paragraph (lets tight lists drop the <p>). */
  para?: string;
  blankBefore: boolean;
}

interface State {
  excerpt: string | null;
}

const FENCE_RE = /^( {0,3})(`{3,}|~{3,})(.*)$/;
const ATX_RE = /^ {0,3}(#{1,6})(?:[ \t]+(.*)|[ \t]*)$/;
const HR_RE = /^ {0,3}([-*_])(?:[ \t]*\1){2,}[ \t]*$/;
const LIST_RE = /^( {0,3})([-*+]|\d{1,9}[.)])( +|$)(.*)$/;
const QUOTE_RE = /^ {0,3}>/;

const isBlank = (l: string): boolean => l.trim() === '';
const indentOf = (l: string): number => /^ */.exec(l)?.[0].length ?? 0;
const isHr = (l: string): boolean => HR_RE.test(l);
const listKey = (m: RegExpExecArray): string => (m[2] as string).slice(-1);

/** Can this line interrupt a paragraph? */
function interruptsParagraph(l: string): boolean {
  const fence = FENCE_RE.exec(l);
  if (fence && !(fence[2]?.startsWith('`') && (fence[3] ?? '').includes('`'))) return true;
  if (ATX_RE.test(l) || isHr(l) || QUOTE_RE.test(l)) return true;
  const m = LIST_RE.exec(l);
  return m !== null && (m[4] ?? '') !== '' && (/^[-*+]/.test(m[2] as string) || parseInt(m[2] as string, 10) === 1);
}

function makeParagraph(lines: string[], st: State): Block {
  const text = lines.map((l) => l.trimStart()).join('\n').trimEnd();
  const nodes = parseInline(text, 0);
  if (st.excerpt === null) {
    const plain = nodesToText(nodes).replace(/\s+/g, ' ').trim();
    if (plain !== '') st.excerpt = plain;
  }
  const inner = nodesToHtml(nodes);
  return { html: `<p>${inner}</p>`, para: inner, blankBefore: false };
}

function parseBlocks(lines: string[], st: State, depth: number): Block[] {
  if (depth > MAX_DEPTH) {
    const text = lines.filter((l) => !isBlank(l));
    return text.length ? [makeParagraph(text, st)] : [];
  }
  const out: Block[] = [];
  let blank = false;
  const push = (b: Block): void => {
    out.push({ ...b, blankBefore: blank && out.length > 0 });
    blank = false;
  };

  let i = 0;
  while (i < lines.length) {
    const line = lines[i] as string;
    if (isBlank(line)) {
      blank = true;
      i++;
      continue;
    }

    // fenced code
    const fence = FENCE_RE.exec(line);
    if (fence && !(fence[2]?.startsWith('`') && (fence[3] ?? '').includes('`'))) {
      const indent = fence[1]?.length ?? 0;
      const marker = fence[2] as string;
      const closeRe = new RegExp(`^ {0,3}${marker[0] === '`' ? '`' : '~'}{${marker.length},}[ \\t]*$`);
      const info = (fence[3] ?? '').trim().split(/\s+/)[0] ?? '';
      const body: string[] = [];
      i++;
      while (i < lines.length && !closeRe.test(lines[i] as string)) {
        const l = lines[i] as string;
        body.push(l.slice(Math.min(indent, indentOf(l))));
        i++;
      }
      i++; // closing fence (or past the end for an unterminated one)
      push(codeBlock(body, info));
      continue;
    }

    // ATX heading
    const atx = ATX_RE.exec(line);
    if (atx) {
      const level = (atx[1] as string).length;
      const content = (atx[2] ?? '').replace(/(^|[ \t]+)#+[ \t]*$/, '').trim();
      push(heading(level, content));
      i++;
      continue;
    }

    if (isHr(line)) {
      push({ html: '<hr>', blankBefore: false });
      i++;
      continue;
    }

    // blockquote
    if (QUOTE_RE.test(line)) {
      const inner: string[] = [];
      while (i < lines.length) {
        const l = lines[i] as string;
        if (QUOTE_RE.test(l)) {
          inner.push(l.replace(/^ {0,3}> ?/, ''));
        } else if (!isBlank(l) && inner.length > 0 && !isBlank(inner[inner.length - 1] as string) && !interruptsParagraph(l)) {
          inner.push(l); // lazy continuation
        } else break;
        i++;
      }
      const blocks = parseBlocks(inner, st, depth + 1);
      push({ html: `<blockquote>\n${blocks.map((b) => b.html + '\n').join('')}</blockquote>`, blankBefore: false });
      continue;
    }

    // list
    const lm = LIST_RE.exec(line);
    if (lm) {
      const res = parseList(lines, i, st, depth);
      push(res.block);
      i = res.next;
      continue;
    }

    // indented code
    if (indentOf(line) >= 4) {
      const body: string[] = [];
      while (i < lines.length && (isBlank(lines[i] as string) || indentOf(lines[i] as string) >= 4)) {
        body.push((lines[i] as string).slice(4));
        i++;
      }
      while (body.length && isBlank(body[body.length - 1] as string)) body.pop();
      push(codeBlock(body, ''));
      continue;
    }

    // paragraph / setext heading
    const para: string[] = [line];
    i++;
    let setext = 0;
    while (i < lines.length) {
      const l = lines[i] as string;
      if (isBlank(l)) break;
      if (/^ {0,3}=+[ \t]*$/.test(l)) setext = 1;
      else if (/^ {0,3}-+[ \t]*$/.test(l)) setext = 2;
      if (setext) {
        i++;
        break;
      }
      if (interruptsParagraph(l)) break;
      para.push(l);
      i++;
    }
    if (setext) {
      const text = para.map((l) => l.trim()).join('\n');
      push(heading(setext, text, para));
    } else {
      push(makeParagraph(para, st));
    }
  }
  return out;

  function heading(level: number, content: string, src?: string[]): Block {
    if (src) {
      const p = makeParagraph(src, st); // records the excerpt like a paragraph
      return { html: `<h${level}>${p.para}</h${level}>`, blankBefore: false };
    }
    return { html: `<h${level}>${nodesToHtml(parseInline(content, 0))}</h${level}>`, blankBefore: false };
  }
}

function codeBlock(body: string[], info: string): Block {
  const cls = /^[A-Za-z0-9_+-]+$/.test(info) ? ` class="language-${info}"` : '';
  const text = body.length ? escapeHtml(body.join('\n')) + '\n' : '';
  return { html: `<pre><code${cls}>${text}</code></pre>`, blankBefore: false };
}

function parseList(lines: string[], start: number, st: State, depth: number): { block: Block; next: number } {
  const first = LIST_RE.exec(lines[start] as string) as RegExpExecArray;
  const key = listKey(first);
  const ordered = /\d/.test(first[2] as string);
  const startNum = ordered ? parseInt(first[2] as string, 10) : 1;
  const items: string[][] = [];
  let loose = false;
  let i = start;

  const siblingAt = (idx: number): RegExpExecArray | null => {
    const l = lines[idx];
    if (l === undefined || isHr(l)) return null;
    const m = LIST_RE.exec(l);
    return m && listKey(m) === key ? m : null;
  };

  while (i < lines.length) {
    const m = siblingAt(i);
    if (!m) break;
    const base = (m[1] as string).length;
    const markerLen = (m[2] as string).length;
    const spaces = (m[3] as string).length;
    let rest = m[4] as string;
    let contentIndent: number;
    if (rest === '') contentIndent = base + markerLen + 1;
    else if (spaces > 4) {
      contentIndent = base + markerLen + 1;
      rest = ' '.repeat(spaces - 1) + rest;
    } else contentIndent = base + markerLen + spaces;

    const body: string[] = [rest];
    i++;
    while (i < lines.length) {
      const l = lines[i] as string;
      if (isBlank(l)) {
        let j = i;
        while (j < lines.length && isBlank(lines[j] as string)) j++;
        if (j < lines.length && indentOf(lines[j] as string) >= contentIndent) {
          for (; i < j; i++) body.push('');
          continue;
        }
        break;
      }
      if (indentOf(l) >= contentIndent) {
        body.push(l.slice(contentIndent));
      } else if (siblingAt(i) || LIST_RE.test(l) || isHr(l) || interruptsParagraph(l) || isBlank(body[body.length - 1] as string)) {
        break;
      } else {
        body.push(l.trimStart()); // lazy continuation
      }
      i++;
    }
    items.push(body);

    let j = i;
    while (j < lines.length && isBlank(lines[j] as string)) j++;
    if (j > i && siblingAt(j)) {
      loose = true;
      i = j;
    } else if (j === i) continue;
    else break;
  }

  const rendered = items.map((body) => parseBlocks(body, st, depth + 1));
  if (rendered.some((blocks) => blocks.some((b, k) => k > 0 && b.blankBefore))) loose = true;

  const lis = rendered.map((blocks) => {
    if (blocks.length === 0) return '<li></li>';
    let inner = '';
    blocks.forEach((b, k) => {
      if (!loose && b.para !== undefined) inner += (k > 0 ? '\n' : '') + b.para;
      else inner += '\n' + b.html;
    });
    const last = blocks[blocks.length - 1] as Block;
    if (loose || last.para === undefined) inner += '\n';
    return `<li>${inner}</li>`;
  });
  const tag = ordered ? 'ol' : 'ul';
  const attr = ordered && startNum !== 1 ? ` start="${startNum}"` : '';
  return { block: { html: `<${tag}${attr}>\n${lis.join('\n')}\n</${tag}>`, blankBefore: false }, next: i };
}

// ------------------------------------------------------------------ excerpt

function truncate(text: string): string {
  if (text.length <= EXCERPT_MAX) return text;
  let cut = text.slice(0, EXCERPT_MAX - 1);
  if (text[EXCERPT_MAX - 1] !== ' ') {
    const sp = cut.lastIndexOf(' ');
    if (sp > 0) cut = cut.slice(0, sp);
  }
  cut = cut.trimEnd();
  if (/[\ud800-\udbff]$/.test(cut)) cut = cut.slice(0, -1);
  return cut + '…';
}

export function renderMarkdown(src: string): MarkdownResult {
  const normalized = src
    .replace(/\r\n?/g, '\n')
    .replace(/\u0000/g, '�')
    .split('\n')
    .map((l) => l.replace(/^[ \t]+/, (ws) => ws.replace(/\t/g, '    ')));
  const st: State = { excerpt: null };
  const blocks = parseBlocks(normalized, st, 0);
  const html = blocks.map((b) => b.html + '\n').join('');
  return { html, excerpt: truncate(st.excerpt ?? '') };
}

// References between a site's files: a small, dependency-free scanner for the URLs that HTML and CSS
// files contain, how they resolve to tree paths, and how a reference is rewritten when its target is
// renamed. Used by FINGERPRINT (docs/LANGUAGE.md, "Fingerprinting"). Ported as rust/src/links.rs: both
// must find the same references (a differential fixture checks this, rust/tests/fixtures/links/).
//
// The scanner is deliberately not a full HTML or CSS parser. It finds:
// - HTML: the attributes `href` (every element but <base>), `src`, `srcset` and `imagesrcset` (each
//   candidate URL), `poster`, `xlink:href`, and `data` on <object>; the CSS inside <style> elements and
//   `style` attributes. Comments, <!…> and <?…> declarations, end tags, and the contents of <script>,
//   <textarea>, <title> and <xmp> are skipped. Attribute values have `&amp;`, `&lt;`, `&gt;`, `&quot;`,
//   `&apos;` and numeric character references (with their `;`) decoded, and are trimmed of ASCII
//   whitespace; other references stay as written. `<base href>` is not honoured.
// - CSS: `url(…)` (quoted or not) and the string after `@import`; comments are skipped and CSS escapes
//   in strings and URLs are decoded.
// All syntax is ASCII, so the TypeScript (UTF-16) and Rust (UTF-8) scanners agree; offsets are each
// implementation's own string indices, lines count `\n` (1-based).

/** One URL found in a file. */
export interface Reference {
  /** Offsets of the URL as written in the scanned text (`raw` is `text.slice(start, end)`). */
  readonly start: number;
  readonly end: number;
  /** The URL as written. */
  readonly raw: string;
  /** The URL with character references (HTML) or escapes (CSS) decoded and ASCII whitespace trimmed. */
  readonly url: string;
  /** 1-based line of `start`. */
  readonly line: number;
  /** Written as CSS (a CSS file, a <style> element or a `style` attribute), so CSS escapes apply. */
  readonly css: boolean;
  /** For a URL in an HTML attribute (not CSS): the lower-case element and attribute names, `img src`. */
  readonly element?: string;
}

const isWs = (c: string | undefined): boolean => c === ' ' || c === '\t' || c === '\n' || c === '\r' || c === '\f';
const isAlpha = (c: string | undefined): boolean => c !== undefined && ((c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z'));
const isHex = (c: string | undefined): boolean => c !== undefined && ((c >= '0' && c <= '9') || (c >= 'a' && c <= 'f') || (c >= 'A' && c <= 'F'));
/** CSS name characters (ASCII letters, digits, `-`, `_`, anything non-ASCII). */
const isNameChar = (c: string | undefined): boolean => c !== undefined && (isAlpha(c) || (c >= '0' && c <= '9') || c === '-' || c === '_' || c.charCodeAt(0) >= 0x80);
const lowerAscii = (s: string): string => s.replace(/[A-Z]/g, (c) => c.toLowerCase());

function trimWs(s: string): [number, number] {
  let a = 0;
  let b = s.length;
  while (a < b && isWs(s[a])) a++;
  while (b > a && isWs(s[b - 1])) b--;
  return [a, b];
}

/** A code point from a character reference or CSS escape: 0, surrogates and values above U+10FFFF become U+FFFD. */
function codePoint(n: number): string {
  return n === 0 || (n >= 0xd800 && n <= 0xdfff) || n > 0x10ffff ? '�' : String.fromCodePoint(n);
}

const NAMED: Record<string, string> = { amp: '&', lt: '<', gt: '>', quot: '"', apos: "'" };

/** Decoded text, and for each of its offsets (and its end) the offset in the source it came from. */
interface Mapped {
  readonly text: string;
  readonly map: readonly number[];
}

/** Decode the character references this scanner knows (see the top of the file); others stay as written. */
export function decodeHtml(s: string): Mapped {
  let out = '';
  const map: number[] = [];
  let i = 0;
  while (i < s.length) {
    if (s[i] !== '&') {
      map.push(i);
      out += s[i++];
      continue;
    }
    const semi = s.indexOf(';', i + 1);
    const name = semi === -1 ? '' : s.slice(i + 1, semi);
    let decoded: string | undefined;
    if (Object.hasOwn(NAMED, name)) decoded = NAMED[name];
    else if (/^#[0-9]{1,8}$/.test(name)) decoded = codePoint(parseInt(name.slice(1), 10));
    else if (/^#[xX][0-9a-fA-F]{1,8}$/.test(name)) decoded = codePoint(parseInt(name.slice(2), 16));
    if (decoded === undefined) {
      map.push(i);
      out += '&';
      i++;
    } else {
      for (let k = 0; k < decoded.length; k++) map.push(i);
      out += decoded;
      i = semi + 1;
    }
  }
  map.push(s.length);
  return { text: out, map };
}

export const decodeHtmlText = (s: string): string => (s.includes('&') ? decodeHtml(s).text : s);

/** Decode CSS escapes: `\` + newline is removed, `\` + 1-6 hex digits (and one following whitespace) is a code point, `\` + anything else is that character. */
export function decodeCss(s: string): Mapped {
  let out = '';
  const map: number[] = [];
  let i = 0;
  while (i < s.length) {
    const c = s[i]!;
    if (c !== '\\') {
      map.push(i);
      out += c;
      i++;
      continue;
    }
    const at = i;
    i++;
    if (i >= s.length) break;
    if (s[i] === '\n') {
      i++;
      continue;
    }
    if (isHex(s[i])) {
      let j = i;
      while (j < s.length && j - i < 6 && isHex(s[j])) j++;
      const ch = codePoint(parseInt(s.slice(i, j), 16));
      for (let k = 0; k < ch.length; k++) map.push(at);
      out += ch;
      i = j;
      if (s[i] === '\r' && s[i + 1] === '\n') i += 2;
      else if (isWs(s[i])) i++;
      continue;
    }
    // The escaped character, whole (both halves of a surrogate pair).
    const ch = String.fromCodePoint(s.codePointAt(i)!);
    for (let k = 0; k < ch.length; k++) map.push(at);
    out += ch;
    i += ch.length;
  }
  map.push(s.length);
  return { text: out, map };
}

export const decodeCssText = (s: string): string => (s.includes('\\') ? decodeCss(s).text : s);

/** Line numbers by offset. */
function lineIndex(text: string): (pos: number) => number {
  const starts: number[] = [];
  for (let i = text.indexOf('\n'); i !== -1; i = text.indexOf('\n', i + 1)) starts.push(i);
  return (pos) => {
    let lo = 0;
    let hi = starts.length;
    while (lo < hi) {
      const mid = (lo + hi) >> 1;
      if (starts[mid]! < pos) lo = mid + 1;
      else hi = mid;
    }
    return lo + 1;
  };
}

type Push = (start: number, end: number, url: string, css: boolean, element?: string) => void;

function collector(text: string, out: Reference[]): Push {
  const lineAt = lineIndex(text);
  return (start, end, url, css, element) => out.push({ start, end, raw: text.slice(start, end), url, line: lineAt(start), css, ...(element !== undefined ? { element } : {}) });
}

// ---------------------------------------------------------------- CSS

/** The end of a CSS string starting at the quote `at`: the index of its closing quote, or undefined (newline or end of input). */
function cssStringEnd(text: string, at: number, limit: number): number | undefined {
  const q = text[at];
  let i = at + 1;
  while (i < limit) {
    const c = text[i];
    if (c === '\\') i += 2;
    else if (c === q) return i;
    else if (c === '\n') return undefined;
    else i++;
  }
  return undefined;
}

/** References in `text[from, to)` read as CSS. */
function scanCssRange(text: string, from: number, to: number, push: Push): void {
  let i = from;
  let afterImport = false;
  while (i < to) {
    const c = text[i]!;
    if (c === '/' && text[i + 1] === '*' && i + 1 < to) {
      const j = text.indexOf('*/', i + 2);
      i = j === -1 || j + 2 > to ? to : j + 2;
      continue;
    }
    if (c === '"' || c === "'") {
      const end = cssStringEnd(text, i, to);
      if (end === undefined) {
        // An unterminated string ends at the newline (or the end).
        const nl = text.indexOf('\n', i + 1);
        i = nl === -1 || nl >= to ? to : nl;
        afterImport = false;
        continue;
      }
      if (afterImport) push(i + 1, end, trimmed(decodeCssText(text.slice(i + 1, end))), true);
      afterImport = false;
      i = end + 1;
      continue;
    }
    if (c === '\\') {
      // The backslash and the whole character it escapes.
      i += 1 + ((text.codePointAt(i + 1) ?? 0) > 0xffff ? 2 : 1);
      continue;
    }
    if (c === '@') {
      let j = i + 1;
      while (j < to && isNameChar(text[j])) j++;
      afterImport = lowerAscii(text.slice(i + 1, j)) === 'import';
      i = j;
      continue;
    }
    if (isNameChar(c)) {
      let j = i;
      while (j < to && isNameChar(text[j])) j++;
      if (j < to && text[j] === '(' && lowerAscii(text.slice(i, j)) === 'url') {
        i = cssUrl(text, j + 1, to, push);
        afterImport = false;
        continue;
      }
      i = j;
      continue;
    }
    if (c === ';' || c === '{' || c === '}') afterImport = false;
    i++;
  }
}

function trimmed(s: string): string {
  const [a, b] = trimWs(s);
  return s.slice(a, b);
}

/** The contents of `url(` starting at `i` (after the parenthesis); returns where scanning continues. */
function cssUrl(text: string, i: number, to: number, push: Push): number {
  while (i < to && isWs(text[i])) i++;
  if (i >= to) return to;
  const q = text[i];
  if (q === '"' || q === "'") {
    const end = cssStringEnd(text, i, to);
    if (end === undefined) return i + 1;
    let k = end + 1;
    while (k < to && isWs(text[k])) k++;
    if (text[k] === ')' && k < to) {
      push(i + 1, end, trimmed(decodeCssText(text.slice(i + 1, end))), true);
      return k + 1;
    }
    return end + 1;
  }
  const start = i;
  while (i < to) {
    const c = text[i]!;
    if (c === ')' || isWs(c) || c === '"' || c === "'" || c === '(') break;
    i += c === '\\' ? 2 : 1;
  }
  if (i > to) i = to;
  const end = i;
  while (i < to && isWs(text[i])) i++;
  if (i < to && text[i] === ')') {
    if (end > start) push(start, end, decodeCssText(text.slice(start, end)), true);
    return i + 1;
  }
  return i;
}

/** The URLs a CSS file references, in order. */
export function scanCss(text: string): Reference[] {
  const out: Reference[] = [];
  scanCssRange(text, 0, text.length, collector(text, out));
  return out;
}

// ---------------------------------------------------------------- HTML

/** Elements whose contents are not markup. `<style>` is read as CSS. */
const RAW_TEXT = new Set(['script', 'style', 'textarea', 'title', 'xmp']);

/** srcset candidates: the offsets of each URL in `value` (spec "parse a srcset attribute", without descriptors). */
function srcsetUrls(value: string): Array<[number, number]> {
  const out: Array<[number, number]> = [];
  let i = 0;
  while (i < value.length) {
    while (i < value.length && (isWs(value[i]) || value[i] === ',')) i++;
    if (i >= value.length) break;
    const start = i;
    while (i < value.length && !isWs(value[i])) i++;
    let end = i;
    if (value[end - 1] === ',') {
      while (end > start && value[end - 1] === ',') end--;
    } else {
      let depth = 0;
      while (i < value.length) {
        const c = value[i];
        if (c === '(') depth++;
        else if (c === ')' && depth > 0) depth--;
        else if (c === ',' && depth === 0) break;
        i++;
      }
      i++;
    }
    if (end > start) out.push([start, end]);
  }
  return out;
}

/** The URLs an HTML file references, in order (including those of its CSS). */
export function scanHtml(text: string): Reference[] {
  const out: Reference[] = [];
  const push = collector(text, out);
  const n = text.length;
  const attribute = (tag: string, name: string, start: number, end: number): void => {
    const value = text.slice(start, end);
    if (name === 'style') {
      scanCssRange(text, start, end, push);
      return;
    }
    if (name === 'srcset' || name === 'imagesrcset') {
      for (const [a, b] of srcsetUrls(value)) push(start + a, start + b, decodeHtmlText(value.slice(a, b)), false, `${tag} ${name}`);
      return;
    }
    const url = name === 'src' || name === 'poster' || name === 'xlink:href' || (name === 'href' && tag !== 'base') || (name === 'data' && tag === 'object');
    if (!url) return;
    const [a, b] = trimWs(value);
    push(start + a, start + b, trimmed(decodeHtmlText(value.slice(a, b))), false, `${tag} ${name}`);
  };
  let i = 0;
  while (i < n) {
    const lt = text.indexOf('<', i);
    if (lt === -1) break;
    i = lt;
    const next = text[i + 1];
    if (text.startsWith('<!--', i)) {
      const j = text.indexOf('-->', i + 4);
      i = j === -1 ? n : j + 3;
      continue;
    }
    if (next === '!' || next === '?' || next === '/') {
      const j = text.indexOf('>', i + 2);
      i = j === -1 ? n : j + 1;
      continue;
    }
    if (!isAlpha(next)) {
      i++;
      continue;
    }
    // A start tag.
    let j = i + 1;
    while (j < n && !isWs(text[j]) && text[j] !== '/' && text[j] !== '>') j++;
    const tag = lowerAscii(text.slice(i + 1, j));
    i = j;
    for (;;) {
      while (i < n && (isWs(text[i]) || text[i] === '/')) i++;
      if (i >= n) break;
      if (text[i] === '>') {
        i++;
        break;
      }
      const nameStart = i;
      i++; // the first character may be `=`
      while (i < n && !isWs(text[i]) && text[i] !== '/' && text[i] !== '>' && text[i] !== '=') i++;
      const name = lowerAscii(text.slice(nameStart, i));
      let k = i;
      while (k < n && isWs(text[k])) k++;
      if (text[k] !== '=') continue;
      i = k + 1;
      while (i < n && isWs(text[i])) i++;
      const q = text[i];
      let start: number;
      let end: number;
      if (q === '"' || q === "'") {
        start = i + 1;
        const close = text.indexOf(q, start);
        end = close === -1 ? n : close;
        i = close === -1 ? n : close + 1;
      } else {
        start = i;
        while (i < n && !isWs(text[i]) && text[i] !== '>') i++;
        end = i;
      }
      attribute(tag, name, start, end);
    }
    if (RAW_TEXT.has(tag)) {
      const close = indexOfCloseTag(text, tag, i);
      if (tag === 'style') scanCssRange(text, i, close, push);
      i = close;
    }
  }
  return out;
}

/** A tag that loads a subresource Subresource Integrity can check: where to add the attribute, and the URL. */
export interface IntegrityTag {
  /** The offset just after the tag name (`<script|`), where ` integrity="…"` goes. */
  readonly at: number;
  /** The URL (`src` of a script, `href` of a link), decoded and trimmed as {@link scanHtml} does. */
  readonly url: string;
}

/**
 * The start tags of an HTML text that load a script or stylesheet SRI applies to: `<script src>` and
 * `<link href>` whose `rel` has the token `stylesheet` or `modulepreload` (ASCII case-insensitive).
 * Tags that already have an `integrity` attribute are left out. Comments, declarations and raw text are
 * skipped as in {@link scanHtml}; when an attribute is given twice, the first one counts.
 */
export function integrityTags(text: string): IntegrityTag[] {
  const out: IntegrityTag[] = [];
  const n = text.length;
  let i = 0;
  while (i < n) {
    const lt = text.indexOf('<', i);
    if (lt === -1) break;
    i = lt;
    const next = text[i + 1];
    if (text.startsWith('<!--', i)) {
      const j = text.indexOf('-->', i + 4);
      i = j === -1 ? n : j + 3;
      continue;
    }
    if (next === '!' || next === '?' || next === '/') {
      const j = text.indexOf('>', i + 2);
      i = j === -1 ? n : j + 1;
      continue;
    }
    if (!isAlpha(next)) {
      i++;
      continue;
    }
    let j = i + 1;
    while (j < n && !isWs(text[j]) && text[j] !== '/' && text[j] !== '>') j++;
    const tag = lowerAscii(text.slice(i + 1, j));
    const at = j;
    i = j;
    const attrs = new Map<string, string>();
    for (;;) {
      while (i < n && (isWs(text[i]) || text[i] === '/')) i++;
      if (i >= n) break;
      if (text[i] === '>') {
        i++;
        break;
      }
      const nameStart = i;
      i++;
      while (i < n && !isWs(text[i]) && text[i] !== '/' && text[i] !== '>' && text[i] !== '=') i++;
      const name = lowerAscii(text.slice(nameStart, i));
      let k = i;
      while (k < n && isWs(text[k])) k++;
      if (text[k] !== '=') {
        if (!attrs.has(name)) attrs.set(name, '');
        continue;
      }
      i = k + 1;
      while (i < n && isWs(text[i])) i++;
      const q = text[i];
      let start: number;
      let end: number;
      if (q === '"' || q === "'") {
        start = i + 1;
        const close = text.indexOf(q, start);
        end = close === -1 ? n : close;
        i = close === -1 ? n : close + 1;
      } else {
        start = i;
        while (i < n && !isWs(text[i]) && text[i] !== '>') i++;
        end = i;
      }
      if (!attrs.has(name)) attrs.set(name, text.slice(start, end));
    }
    if (!attrs.has('integrity')) {
      let raw: string | undefined;
      if (tag === 'script') raw = attrs.get('src');
      else if (tag === 'link') {
        const rel = lowerAscii(decodeHtmlText(attrs.get('rel') ?? '')).split(/[ \t\n\r\f]+/);
        if (rel.includes('stylesheet') || rel.includes('modulepreload')) raw = attrs.get('href');
      }
      if (raw !== undefined) {
        const [a, b] = trimWs(raw);
        const url = trimmed(decodeHtmlText(raw.slice(a, b)));
        if (url !== '') out.push({ at, url });
      }
    }
    if (RAW_TEXT.has(tag)) i = indexOfCloseTag(text, tag, i);
  }
  return out;
}

/**
 * Where a canonical link goes in an HTML text: the offset of its first `</head` end tag, `'has'` when a
 * `<link>` whose `rel` has the token `canonical` (ASCII case-insensitive) comes before it, or undefined when
 * there is no `</head>`. Comments, declarations and raw text are skipped as in {@link scanHtml}.
 */
export function canonicalSlot(text: string): number | 'has' | undefined {
  const n = text.length;
  let i = 0;
  while (i < n) {
    const lt = text.indexOf('<', i);
    if (lt === -1) break;
    i = lt;
    const next = text[i + 1];
    if (text.startsWith('<!--', i)) {
      const j = text.indexOf('-->', i + 4);
      i = j === -1 ? n : j + 3;
      continue;
    }
    if (next === '/') {
      const end = i + 6;
      if (lowerAscii(text.slice(i + 2, end)) === 'head' && (end >= n || isWs(text[end]) || text[end] === '/' || text[end] === '>')) return i;
    }
    if (next === '!' || next === '?' || next === '/') {
      const j = text.indexOf('>', i + 2);
      i = j === -1 ? n : j + 1;
      continue;
    }
    if (!isAlpha(next)) {
      i++;
      continue;
    }
    let j = i + 1;
    while (j < n && !isWs(text[j]) && text[j] !== '/' && text[j] !== '>') j++;
    const tag = lowerAscii(text.slice(i + 1, j));
    i = j;
    let rel: string | undefined;
    for (;;) {
      while (i < n && (isWs(text[i]) || text[i] === '/')) i++;
      if (i >= n) break;
      if (text[i] === '>') {
        i++;
        break;
      }
      const nameStart = i;
      i++;
      while (i < n && !isWs(text[i]) && text[i] !== '/' && text[i] !== '>' && text[i] !== '=') i++;
      const name = lowerAscii(text.slice(nameStart, i));
      let k = i;
      while (k < n && isWs(text[k])) k++;
      if (text[k] !== '=') continue;
      i = k + 1;
      while (i < n && isWs(text[i])) i++;
      const q = text[i];
      let start: number;
      let end: number;
      if (q === '"' || q === "'") {
        start = i + 1;
        const close = text.indexOf(q, start);
        end = close === -1 ? n : close;
        i = close === -1 ? n : close + 1;
      } else {
        start = i;
        while (i < n && !isWs(text[i]) && text[i] !== '>') i++;
        end = i;
      }
      if (name === 'rel' && rel === undefined) rel = text.slice(start, end);
    }
    if (tag === 'link' && rel !== undefined && lowerAscii(decodeHtmlText(rel)).split(/[ \t\n\r\f]+/).includes('canonical')) return 'has';
    if (RAW_TEXT.has(tag)) i = indexOfCloseTag(text, tag, i);
  }
  return undefined;
}

/** What CSP needs from a page: its inline script and style texts, in order, and where its `<head>` start tag ends. */
export interface InlineBlocks {
  /** The contents of each `<script>` without a `src` attribute (also empty ones). */
  readonly scripts: readonly string[];
  /** The contents of each `<style>`. */
  readonly styles: readonly string[];
  /** The offset just after the first `<head …>` start tag, if there is one. */
  readonly headEnd?: number;
}

/** The inline scripts and styles of an HTML text (comments and other raw text skipped as in {@link scanHtml}). */
export function inlineBlocks(text: string): InlineBlocks {
  const scripts: string[] = [];
  const styles: string[] = [];
  let headEnd: number | undefined;
  const n = text.length;
  let i = 0;
  while (i < n) {
    const lt = text.indexOf('<', i);
    if (lt === -1) break;
    i = lt;
    const next = text[i + 1];
    if (text.startsWith('<!--', i)) {
      const j = text.indexOf('-->', i + 4);
      i = j === -1 ? n : j + 3;
      continue;
    }
    if (next === '!' || next === '?' || next === '/') {
      const j = text.indexOf('>', i + 2);
      i = j === -1 ? n : j + 1;
      continue;
    }
    if (!isAlpha(next)) {
      i++;
      continue;
    }
    let j = i + 1;
    while (j < n && !isWs(text[j]) && text[j] !== '/' && text[j] !== '>') j++;
    const tag = lowerAscii(text.slice(i + 1, j));
    i = j;
    let src = false;
    let closed = false;
    for (;;) {
      while (i < n && (isWs(text[i]) || text[i] === '/')) i++;
      if (i >= n) break;
      if (text[i] === '>') {
        i++;
        closed = true;
        break;
      }
      const nameStart = i;
      i++;
      while (i < n && !isWs(text[i]) && text[i] !== '/' && text[i] !== '>' && text[i] !== '=') i++;
      if (lowerAscii(text.slice(nameStart, i)) === 'src') src = true;
      let k = i;
      while (k < n && isWs(text[k])) k++;
      if (text[k] !== '=') continue;
      i = k + 1;
      while (i < n && isWs(text[i])) i++;
      const q = text[i];
      if (q === '"' || q === "'") {
        const close = text.indexOf(q, i + 1);
        i = close === -1 ? n : close + 1;
      } else {
        while (i < n && !isWs(text[i]) && text[i] !== '>') i++;
      }
    }
    if (tag === 'head' && headEnd === undefined && closed) headEnd = i;
    if (RAW_TEXT.has(tag)) {
      const close = indexOfCloseTag(text, tag, i);
      if (closed && tag === 'script' && !src) scripts.push(text.slice(i, close));
      if (closed && tag === 'style') styles.push(text.slice(i, close));
      i = close;
    }
  }
  return { scripts, styles, ...(headEnd !== undefined ? { headEnd } : {}) };
}

/** Where `</tag` (ASCII case-insensitive, followed by whitespace, `/` or `>`) starts at or after `from`; the end of the text if nowhere. */
function indexOfCloseTag(text: string, tag: string, from: number): number {
  let i = from;
  for (;;) {
    const j = text.indexOf('</', i);
    if (j === -1) return text.length;
    const after = j + 2 + tag.length;
    if (lowerAscii(text.slice(j + 2, after)) === tag && (after >= text.length || isWs(text[after]) || text[after] === '/' || text[after] === '>')) return j;
    i = j + 2;
  }
}

/** Whether a path is read by {@link scanFile}, and how. */
export function referenceKind(path: string): 'html' | 'css' | undefined {
  const lower = lowerAscii(path);
  if (lower.endsWith('.html') || lower.endsWith('.htm')) return 'html';
  if (lower.endsWith('.css')) return 'css';
  return undefined;
}

const UTF8 = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true });

/** A file's text and references; undefined when it is not HTML or CSS, or not valid UTF-8. */
export function scanFile(path: string, bytes: Uint8Array): { text: string; refs: Reference[] } | undefined {
  const kind = referenceKind(path);
  if (kind === undefined) return undefined;
  let text: string;
  try {
    text = UTF8.decode(bytes);
  } catch {
    return undefined;
  }
  return { text, refs: kind === 'html' ? scanHtml(text) : scanCss(text) };
}

// ---------------------------------------------------------------- resolution

/** What a reference points at. */
export type Target =
  /** Nothing to check: empty, a fragment or query of the same document, or a non-http(s) scheme (mailto:, data:, …). */
  | { readonly kind: 'none' }
  /** An http(s) URL or a protocol-relative `//host/…`. */
  | { readonly kind: 'external'; readonly url: string }
  /**
   * A file of the site: `path` is absolute, normalized and NFC (`..` stops at the root), percent-decoded;
   * `dir` when the URL ends with `/` (or `.`/`..`), so it names a directory.
   */
  | { readonly kind: 'internal'; readonly path: string; readonly dir: boolean };

const SCHEME = /^[A-Za-z][A-Za-z0-9+.-]*:/;

/** Percent-decode one path segment; a segment whose decoding is not valid UTF-8 stays as written. */
export function percentDecode(seg: string): string {
  if (!seg.includes('%')) return seg;
  const bytes: number[] = [];
  const enc = new TextEncoder();
  for (let i = 0; i < seg.length; ) {
    if (seg[i] === '%' && isHex(seg[i + 1]) && isHex(seg[i + 2])) {
      bytes.push(parseInt(seg.slice(i + 1, i + 3), 16));
      i += 3;
    } else {
      const cp = seg.codePointAt(i)!;
      const ch = String.fromCodePoint(cp);
      bytes.push(...enc.encode(ch));
      i += ch.length;
    }
  }
  try {
    return UTF8.decode(new Uint8Array(bytes));
  } catch {
    return seg;
  }
}

/**
 * Resolve a reference's URL against the path of the file that contains it. `root` is the directory the
 * site is served from (the EXPORT path): an absolute URL path (`/img/a.png`) names a file below it, and
 * `..` never climbs above it (as in a browser), for references in files below it; elsewhere `..` stops at `/`.
 */
export function resolveReference(url: string, from: string, root = '/'): Target {
  if (url === '' || url.startsWith('#') || url.startsWith('?')) return { kind: 'none' };
  if (url.startsWith('//')) return { kind: 'external', url };
  const scheme = SCHEME.exec(url);
  if (scheme) {
    const s = scheme[0].toLowerCase();
    return s === 'http:' || s === 'https:' ? { kind: 'external', url } : { kind: 'none' };
  }
  const cut = url.search(/[?#]/);
  const raw = cut === -1 ? url : url.slice(0, cut);
  const rootSegs = root === '/' ? [] : root.split('/').slice(1);
  const below = root === '/' || from.startsWith(root + '/');
  const absolute = raw.startsWith('/');
  const segs: string[] = absolute ? [...rootSegs] : from.split('/').slice(1, -1);
  const floor = absolute || below ? rootSegs.length : 0;
  const parts = absolute ? raw.split('/').slice(1) : raw.split('/');
  let dir = false;
  for (const [i, part] of parts.entries()) {
    const last = i === parts.length - 1;
    const seg = percentDecode(part);
    if (seg === '' || seg === '.') {
      if (last) dir = true;
    } else if (seg === '..') {
      if (segs.length > floor) segs.pop();
      if (last) dir = true;
    } else {
      segs.push(seg);
    }
  }
  if (segs.length === 0) dir = true;
  const path = '/' + segs.join('/');
  return { kind: 'internal', path: /^[\x00-\x7f]*$/.test(path) ? path : path.normalize('NFC'), dir };
}

// ---------------------------------------------------------------- rewriting

/** Percent-encode a path, keeping `/` and the URL-unreserved characters `A-Z a-z 0-9 - . _ ~`. */
export function encodePath(p: string): string {
  let out = '';
  for (const b of new TextEncoder().encode(p)) {
    const c = String.fromCharCode(b);
    out += /[A-Za-z0-9\-._~/]/.test(c) ? c : '%' + b.toString(16).toUpperCase().padStart(2, '0');
  }
  return out;
}

function basename(p: string): string {
  return p.slice(p.lastIndexOf('/') + 1);
}

/** A relative URL from the file `from` to the file `to` (both absolute tree paths), percent-encoded. */
export function relativeUrl(from: string, to: string): string {
  const dir = from.split('/').slice(1, -1);
  const target = to.split('/').slice(1);
  let k = 0;
  while (k < dir.length && k < target.length - 1 && dir[k] === target[k]) k++;
  return '../'.repeat(dir.length - k) + target.slice(k).map(encodePath).join('/');
}

/**
 * The reference `raw` (written in `from`, resolved to `oldPath`) pointing at `newPath` instead, where only the
 * file name changed by the insertion of `.<hash>`. The hash is inserted into the name as written, before the
 * dot of its extension (or after the name), so the directories, the query, the fragment and the way it is
 * written (escapes, character references, percent-encoding) are kept. When the name can't be edited that way
 * (its extension's dot is percent-encoded, say), the path becomes a relative URL from `from` to `newPath`.
 */
export function renameReference(raw: string, css: boolean, from: string, oldPath: string, newPath: string): string {
  const { text, map } = css ? decodeCss(raw) : decodeHtml(raw);
  const [t0, t1] = trimWs(text);
  const url = text.slice(t0, t1);
  const cut = url.search(/[?#]/);
  const pathEnd = t0 + (cut === -1 ? url.length : cut);
  const slash = pathEnd > t0 ? text.lastIndexOf('/', pathEnd - 1) : -1;
  const segStart = slash < t0 ? t0 : slash + 1;
  const seg = text.slice(segStart, pathEnd);
  const oldBase = basename(oldPath);
  const newBase = basename(newPath);
  const insertAt = (offset: number, insert: string): string => raw.slice(0, map[offset]) + insert + raw.slice(map[offset]);
  if (percentDecode(seg) === oldBase) {
    const oldDot = oldBase.lastIndexOf('.');
    if (oldDot > 0) {
      const ext = oldBase.slice(oldDot);
      const d = seg.lastIndexOf('.');
      if (d >= 0 && percentDecode(seg.slice(d)) === ext) return insertAt(segStart + d, newBase.slice(oldDot, newBase.length - ext.length));
    } else {
      return insertAt(pathEnd, newBase.slice(oldBase.length));
    }
  }
  return raw.slice(0, map[t0]) + relativeUrl(from, newPath) + raw.slice(map[pathEnd]);
}

/** `text` with each reference whose target `rename` maps to a new path rewritten; undefined when nothing changed. */
export function rewriteReferences(
  text: string,
  refs: readonly Reference[],
  from: string,
  root: string,
  rename: (path: string) => string | undefined,
): string | undefined {
  let out = '';
  let at = 0;
  let changed = false;
  for (const r of refs) {
    const t = resolveReference(r.url, from, root);
    if (t.kind !== 'internal' || t.dir) continue;
    const to = rename(t.path);
    if (to === undefined) continue;
    out += text.slice(at, r.start) + renameReference(r.raw, r.css, from, t.path, to);
    at = r.end;
    changed = true;
  }
  return changed ? out + text.slice(at) : undefined;
}

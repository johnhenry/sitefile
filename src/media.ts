// Template helpers that write HTML for files of the tree (issue #105): `${file "/img/a.jpg" | img alt="…"}`.
//
// This module knows files, not templates: media types, image sizes read from file headers, URLs,
// subresource integrity, and the markup each helper writes. src/render.ts parses `${file …}` and
// calls renderFileHelper; rust/src/media.rs is the port (same output bytes, errors and messages).
import { createHash } from 'node:crypto';
import { SitefileError, type SourceSpan } from './errors.js';
import { escapeHtml } from './render.js';
import { normalizeVirtual } from './paths.js';

/** Helpers that produce a value (escaped like any substituted value). */
export const VALUE_HELPERS = ['url', 'width', 'height', 'type', 'size', 'integrity', 'datauri'] as const;
/** Helpers that write a tag (trusted markup: inserted as-is; every attribute value is escaped). */
export const TAG_HELPERS = ['img', 'video', 'audio', 'source', 'script', 'stylesheet', 'preload', 'modulepreload', 'pre', 'svg'] as const;
export const FILE_HELPERS: readonly string[] = [...VALUE_HELPERS, ...TAG_HELPERS];

/** The largest file `datauri` inlines. */
export const DATAURI_MAX = 32768;

const MIME: Readonly<Record<string, string>> = {
  png: 'image/png',
  jpg: 'image/jpeg',
  jpeg: 'image/jpeg',
  gif: 'image/gif',
  webp: 'image/webp',
  avif: 'image/avif',
  svg: 'image/svg+xml',
  ico: 'image/x-icon',
  mp4: 'video/mp4',
  m4v: 'video/mp4',
  webm: 'video/webm',
  ogv: 'video/ogg',
  mov: 'video/quicktime',
  mp3: 'audio/mpeg',
  m4a: 'audio/mp4',
  aac: 'audio/aac',
  oga: 'audio/ogg',
  ogg: 'audio/ogg',
  opus: 'audio/opus',
  wav: 'audio/wav',
  flac: 'audio/flac',
  css: 'text/css',
  js: 'text/javascript',
  mjs: 'text/javascript',
  json: 'application/json',
  html: 'text/html',
  htm: 'text/html',
  txt: 'text/plain',
  xml: 'application/xml',
  pdf: 'application/pdf',
  wasm: 'application/wasm',
  woff: 'font/woff',
  woff2: 'font/woff2',
  ttf: 'font/ttf',
  otf: 'font/otf',
};

/** The lowercase extension of a path's last segment ('' if none). */
export function extensionOf(path: string): string {
  const base = path.slice(path.lastIndexOf('/') + 1);
  const dot = base.lastIndexOf('.');
  return dot <= 0 ? '' : base.slice(dot + 1).toLowerCase();
}

/** The media type of a path, by extension (`application/octet-stream` when unknown). */
export function mediaType(path: string): string {
  return MIME[extensionOf(path)] ?? 'application/octet-stream';
}

/** Extensions the image helpers accept (`img`, `width`, `height`, `source` writes `srcset`). */
const IMAGE_EXTENSIONS = new Set(['png', 'jpg', 'jpeg', 'gif', 'webp', 'avif', 'svg']);

export function isImagePath(path: string): boolean {
  return IMAGE_EXTENSIONS.has(extensionOf(path));
}

/** The URL of a tree path: each segment percent-encoded like `encodeURIComponent`. */
export function fileUrl(path: string): string {
  return path.split('/').map(encodeURIComponent).join('/');
}

/** `sha384-<base64>`, the subresource integrity of `bytes`. */
export function integrityOf(bytes: Uint8Array): string {
  return 'sha384-' + createHash('sha384').update(bytes).digest('base64');
}

// ---------------------------------------------------------------- image headers

export interface ImageSize {
  readonly width: number;
  readonly height: number;
}

type Format = 'PNG' | 'JPEG' | 'GIF' | 'WebP' | 'AVIF';

/** Which image format the bytes are, by their magic numbers. */
function sniff(b: Uint8Array): Format | undefined {
  const at = (i: number, s: string): boolean => [...s].every((c, k) => b[i + k] === c.charCodeAt(0));
  if (b.length >= 8 && b[0] === 0x89 && at(1, 'PNG\r\n\x1a\n')) return 'PNG';
  if (b.length >= 3 && b[0] === 0xff && b[1] === 0xd8 && b[2] === 0xff) return 'JPEG';
  if (at(0, 'GIF87a') || at(0, 'GIF89a')) return 'GIF';
  if (at(0, 'RIFF') && at(8, 'WEBP')) return 'WebP';
  if (at(4, 'ftyp') && isAvifBrand(b)) return 'AVIF';
  return undefined;
}

const u16be = (b: Uint8Array, i: number): number => (b[i]! << 8) | b[i + 1]!;
const u16le = (b: Uint8Array, i: number): number => b[i]! | (b[i + 1]! << 8);
const u24le = (b: Uint8Array, i: number): number => b[i]! | (b[i + 1]! << 8) | (b[i + 2]! << 16);
const u32be = (b: Uint8Array, i: number): number => ((b[i]! << 24) >>> 0) + (b[i + 1]! << 16) + (b[i + 2]! << 8) + b[i + 3]!;
const u32le = (b: Uint8Array, i: number): number => b[i]! + (b[i + 1]! << 8) + (b[i + 2]! << 16) + ((b[i + 3]! << 24) >>> 0);

/** Thrown by the readers below; turned into one message per format. */
class Malformed extends Error {}
const need = (b: Uint8Array, end: number): void => {
  if (end > b.length) throw new Malformed();
};

function pngSize(b: Uint8Array): ImageSize {
  need(b, 24);
  if (String.fromCharCode(b[12]!, b[13]!, b[14]!, b[15]!) !== 'IHDR') throw new Malformed();
  return { width: u32be(b, 16), height: u32be(b, 20) };
}

function gifSize(b: Uint8Array): ImageSize {
  need(b, 10);
  return { width: u16le(b, 6), height: u16le(b, 8) };
}

/** JPEG: the first SOF segment; EXIF orientation 5 to 8 (a quarter turn) swaps width and height. */
function jpegSize(b: Uint8Array): ImageSize {
  let i = 2;
  let turned = false;
  for (;;) {
    need(b, i + 2);
    if (b[i] !== 0xff) throw new Malformed();
    let m = b[i + 1]!;
    while (m === 0xff) {
      i++;
      need(b, i + 2);
      m = b[i + 1]!;
    }
    if (m === 0x01 || (m >= 0xd0 && m <= 0xd8)) {
      i += 2;
      continue;
    }
    if (m === 0xd9 || m === 0xda) throw new Malformed(); // end of image or start of scan before any SOF
    need(b, i + 4);
    const len = u16be(b, i + 2);
    if (len < 2) throw new Malformed();
    const end = i + 2 + len;
    need(b, end);
    const sof = (m >= 0xc0 && m <= 0xc3) || (m >= 0xc5 && m <= 0xc7) || (m >= 0xc9 && m <= 0xcb) || (m >= 0xcd && m <= 0xcf);
    if (sof) {
      if (len < 7) throw new Malformed();
      const height = u16be(b, i + 5);
      const width = u16be(b, i + 7);
      return turned ? { width: height, height: width } : { width, height };
    }
    if (m === 0xe1 && !turned) turned = exifQuarterTurn(b.subarray(i + 4, end));
    i = end;
  }
}

/** An APP1 segment's EXIF orientation is 5, 6, 7 or 8. Anything unreadable counts as no orientation. */
function exifQuarterTurn(s: Uint8Array): boolean {
  if (s.length < 14 || String.fromCharCode(...s.subarray(0, 6)) !== 'Exif\0\0') return false;
  const t = s.subarray(6);
  const le = t[0] === 0x49 && t[1] === 0x49;
  if (!le && !(t[0] === 0x4d && t[1] === 0x4d)) return false;
  const u16 = (i: number): number => (le ? u16le(t, i) : u16be(t, i));
  const u32 = (i: number): number => (le ? u32le(t, i) : u32be(t, i));
  if (u16(2) !== 42) return false;
  const ifd = u32(4);
  if (ifd + 2 > t.length) return false;
  const n = u16(ifd);
  for (let k = 0; k < n; k++) {
    const e = ifd + 2 + 12 * k;
    if (e + 12 > t.length) return false;
    if (u16(e) === 0x0112) {
      const v = u16(e + 2) === 3 ? u16(e + 8) : 0;
      return v >= 5 && v <= 8;
    }
  }
  return false;
}

function webpSize(b: Uint8Array): ImageSize {
  need(b, 16);
  const chunk = String.fromCharCode(b[12]!, b[13]!, b[14]!, b[15]!);
  if (chunk === 'VP8 ') {
    need(b, 30);
    if (b[23] !== 0x9d || b[24] !== 0x01 || b[25] !== 0x2a) throw new Malformed();
    return { width: u16le(b, 26) & 0x3fff, height: u16le(b, 28) & 0x3fff };
  }
  if (chunk === 'VP8L') {
    need(b, 25);
    if (b[20] !== 0x2f) throw new Malformed();
    const bits = u32le(b, 21);
    return { width: (bits & 0x3fff) + 1, height: ((bits >>> 14) & 0x3fff) + 1 };
  }
  if (chunk === 'VP8X') {
    need(b, 30);
    return { width: u24le(b, 24) + 1, height: u24le(b, 27) + 1 };
  }
  throw new Malformed();
}

// AVIF: ISO-BMFF boxes. The size is the primary item's `ispe` property; `irot` 1 or 3 swaps it.

interface Box {
  readonly type: string;
  /** Start of the box's payload. */
  readonly start: number;
  readonly end: number;
}

/** The boxes in b[from, to). */
function boxes(b: Uint8Array, from: number, to: number): Box[] {
  const out: Box[] = [];
  let i = from;
  while (i < to) {
    if (i + 8 > to) throw new Malformed();
    let size = u32be(b, i);
    const type = String.fromCharCode(b[i + 4]!, b[i + 5]!, b[i + 6]!, b[i + 7]!);
    let start = i + 8;
    if (size === 1) {
      if (i + 16 > to || u32be(b, i + 8) !== 0) throw new Malformed(); // a 64-bit size over 4 GiB
      size = u32be(b, i + 12);
      start = i + 16;
    } else if (size === 0) {
      size = to - i;
    }
    if (size < start - i || i + size > to) throw new Malformed();
    out.push({ type, start, end: i + size });
    i += size;
  }
  return out;
}

function isAvifBrand(b: Uint8Array): boolean {
  if (b.length < 16) return false;
  const size = u32be(b, 0);
  const end = Math.min(size, b.length);
  for (let i = 8; i + 4 <= end; i += 4) {
    if (i === 12) continue; // minor version
    const brand = String.fromCharCode(b[i]!, b[i + 1]!, b[i + 2]!, b[i + 3]!);
    if (brand === 'avif' || brand === 'avis') return true;
  }
  return false;
}

function avifSize(b: Uint8Array): ImageSize {
  const meta = boxes(b, 0, b.length).find((x) => x.type === 'meta');
  if (!meta || meta.start + 4 > meta.end) throw new Malformed();
  const inner = boxes(b, meta.start + 4, meta.end); // a full box: version and flags first
  const pitm = inner.find((x) => x.type === 'pitm');
  const iprp = inner.find((x) => x.type === 'iprp');
  if (!pitm || !iprp || pitm.start + 6 > pitm.end) throw new Malformed();
  const primary = b[pitm.start] === 0 ? u16be(b, pitm.start + 4) : (need(b, pitm.start + 8), u32be(b, pitm.start + 4));
  const props = boxes(b, iprp.start, iprp.end);
  const ipco = props.find((x) => x.type === 'ipco');
  const ipma = props.find((x) => x.type === 'ipma');
  if (!ipco || !ipma) throw new Malformed();
  const properties = boxes(b, ipco.start, ipco.end);
  // ipma: version, flags, entry_count, then per item: id, count, associations.
  need(b, ipma.start + 8);
  const version = b[ipma.start]!;
  const wide = (b[ipma.start + 3]! & 1) === 1;
  let i = ipma.start + 4;
  const entries = u32be(b, i);
  i += 4;
  let associated: number[] | undefined;
  for (let e = 0; e < entries; e++) {
    const idLen = version < 1 ? 2 : 4;
    if (i + idLen + 1 > ipma.end) throw new Malformed();
    const id = idLen === 2 ? u16be(b, i) : u32be(b, i);
    i += idLen;
    const count = b[i]!;
    i += 1;
    const list: number[] = [];
    for (let a = 0; a < count; a++) {
      if (wide) {
        if (i + 2 > ipma.end) throw new Malformed();
        list.push(u16be(b, i) & 0x7fff);
        i += 2;
      } else {
        if (i + 1 > ipma.end) throw new Malformed();
        list.push(b[i]! & 0x7f);
        i += 1;
      }
    }
    if (id === primary && associated === undefined) associated = list;
  }
  if (!associated) throw new Malformed();
  let size: ImageSize | undefined;
  let turned = false;
  for (const index of associated) {
    const p = properties[index - 1];
    if (!p) continue;
    if (p.type === 'ispe' && !size) {
      if (p.start + 12 > p.end) throw new Malformed();
      size = { width: u32be(b, p.start + 4), height: u32be(b, p.start + 8) };
    } else if (p.type === 'irot' && p.start < p.end) {
      turned = (b[p.start]! & 3) % 2 === 1;
    }
  }
  if (!size) throw new Malformed();
  return turned ? { width: size.height, height: size.width } : size;
}

/**
 * The width and height of an image file, from its header (PNG, JPEG, GIF, WebP, AVIF): bounded reads,
 * no decoding. undefined for an SVG (its size is in its markup, often relative). E_DATA when the
 * bytes are no supported image, or the header is truncated or malformed.
 */
export function imageSize(bytes: Uint8Array, path: string): ImageSize | undefined {
  if (extensionOf(path) === 'svg') return undefined;
  const format = sniff(bytes);
  if (!format) throw new SitefileError('E_DATA', `'${path}' is not a PNG, JPEG, GIF, WebP or AVIF image`);
  let size: ImageSize;
  try {
    size =
      format === 'PNG' ? pngSize(bytes) : format === 'JPEG' ? jpegSize(bytes) : format === 'GIF' ? gifSize(bytes) : format === 'WebP' ? webpSize(bytes) : avifSize(bytes);
  } catch (e) {
    if (!(e instanceof Malformed)) throw e;
    throw new SitefileError('E_DATA', `'${path}' is not a valid ${format} image: its header is truncated or malformed`);
  }
  if (size.width === 0 || size.height === 0) {
    throw new SitefileError('E_DATA', `'${path}' is not a valid ${format} image: its header is truncated or malformed`);
  }
  return size;
}

// ---------------------------------------------------------------- helpers

/** A file of the tree a helper writes markup for. */
export interface TreeFile {
  /** Its normalized absolute tree path. */
  readonly path: string;
  readonly bytes: Uint8Array;
}

/** An attribute written in the template: `alt="…"` (value) or `controls` (bare). */
export interface HelperAttr {
  readonly name: string;
  readonly value?: string;
}

/** What a helper produced: markup (inserted as-is) or a value (escaped like any `${…}`). */
export type HelperOutput = { readonly markup: string } | { readonly value: string };

function decodeText(f: TreeFile, helper: string, span: SourceSpan): string {
  try {
    return new TextDecoder('utf-8', { fatal: true }).decode(f.bytes);
  } catch (e) {
    throw new SitefileError('E_ENCODING', `'${f.path}' is not valid UTF-8 (the ${helper} helper writes its text into the page)`, span, { cause: e });
  }
}

/**
 * `<name` and its attributes: the helper's own (`auto`, in order), then the template's, then the
 * helper's defaults. A template attribute named like an automatic or default one (ASCII
 * case-insensitive) replaces its value in place. Every value is HTML-escaped.
 */
function tag(
  name: string,
  auto: ReadonlyArray<readonly [string, string | undefined]>,
  attrs: readonly HelperAttr[],
  defaults: ReadonlyArray<readonly [string, string]> = [],
): string {
  const given = new Map<string, HelperAttr>();
  for (const a of attrs) given.set(a.name.toLowerCase(), a);
  const own = new Set([...auto.map(([n]) => n), ...defaults.map(([n]) => n)]);
  const one = (n: string, v: string | undefined): string => (v === undefined ? ` ${n}` : ` ${n}="${escapeHtml(v)}"`);
  const pick = (n: string, v: string | undefined): string => {
    const g = given.get(n);
    return g ? one(g.name, g.value) : one(n, v);
  };
  let out = `<${name}`;
  for (const [n, v] of auto) out += pick(n, v);
  for (const a of attrs) if (!own.has(a.name.toLowerCase())) out += one(a.name, a.value);
  for (const [n, v] of defaults) out += pick(n, v);
  return out;
}

/** The files of the stage's tree at a RENDER, as `${file …}` sees them. */
export interface TemplateTree {
  get(path: string): Uint8Array | undefined;
  isDirectory(path: string): boolean;
}

/** `as` of a preload, from the extension. */
const PRELOAD_AS: Readonly<Record<string, string>> = {
  css: 'style', js: 'script', mjs: 'script', woff: 'font', woff2: 'font', ttf: 'font', otf: 'font', json: 'fetch',
  png: 'image', jpg: 'image', jpeg: 'image', gif: 'image', webp: 'image', avif: 'image', svg: 'image',
};

export interface HelperOptions {
  /** RENDER --inline-svg: the svg helper may paste SVG markup into the page. */
  readonly inlineSvg: boolean;
}

/** The file `${file "<raw>"}` names: an absolute path in the tree (E_PATH, E_TYPE, E_NOT_FOUND otherwise). */
export function resolveFile(raw: string, tree: TemplateTree, span: SourceSpan): TreeFile {
  const p = normalizeVirtual(raw, span, '${file} path');
  if (p.path === '/' || tree.isDirectory(p.path)) throw new SitefileError('E_TYPE', `'${p.path}' is a directory; \${file} needs a file`, span);
  const bytes = tree.get(p.path);
  if (!bytes) {
    throw new SitefileError('E_NOT_FOUND', `'${p.path}' is not in the tree at this RENDER (write it with COPY, RENDER or TRANSFORM on an earlier line)`, span);
  }
  return { path: p.path, bytes };
}

/**
 * What `${file "<raw>" | helper attrs…}` writes (`helper` undefined: the file's URL). A value is
 * escaped by the caller like any substituted value; markup is inserted as it is.
 */
export function renderFile(raw: string, helper: string | undefined, attrs: readonly HelperAttr[], tree: TemplateTree, opts: HelperOptions, span: SourceSpan): HelperOutput {
  return renderFileHelper(helper, resolveFile(raw, tree, span), attrs, opts, span);
}

/** What `helper` writes for file `f` (`undefined`: no helper, the file's URL). */
export function renderFileHelper(helper: string | undefined, f: TreeFile, attrs: readonly HelperAttr[], opts: HelperOptions, span: SourceSpan): HelperOutput {
  const url = fileUrl(f.path);
  const ext = extensionOf(f.path);
  const image = (what: string): ImageSize | undefined => {
    if (!IMAGE_EXTENSIONS.has(ext)) {
      throw new SitefileError('E_TYPE', `'${f.path}' is not an image (${what} reads .png, .jpg, .jpeg, .gif, .webp, .avif and .svg files)`, span);
    }
    try {
      return imageSize(f.bytes, f.path);
    } catch (e) {
      if (e instanceof SitefileError && e.span === undefined) throw new SitefileError(e.code, e.detail, span, { cause: e });
      throw e;
    }
  };
  switch (helper) {
    case undefined:
    case 'url':
      return { value: url };
    case 'type':
      return { value: mediaType(f.path) };
    case 'size':
      return { value: String(f.bytes.length) };
    case 'integrity':
      return { value: integrityOf(f.bytes) };
    case 'width':
    case 'height': {
      const size = image(helper);
      if (!size) throw new SitefileError('E_TYPE', `'${f.path}' is an SVG image; its ${helper} is in its markup, not in a header`, span);
      return { value: String(helper === 'width' ? size.width : size.height) };
    }
    case 'datauri':
      if (f.bytes.length > DATAURI_MAX) {
        throw new SitefileError('E_TOO_LARGE', `'${f.path}' is ${f.bytes.length} bytes; datauri inlines files of at most ${DATAURI_MAX} bytes (link to it instead)`, span);
      }
      return { value: `data:${mediaType(f.path)};base64,${Buffer.from(f.bytes).toString('base64')}` };
    case 'img': {
      const size = image('img');
      const auto: Array<readonly [string, string]> = [['src', url]];
      if (size) auto.push(['width', String(size.width)], ['height', String(size.height)]);
      return { markup: tag('img', auto, attrs, [['loading', 'lazy'], ['decoding', 'async']]) + '>' };
    }
    case 'video':
    case 'audio':
      return { markup: tag(helper, [['src', url]], attrs) + `></${helper}>` };
    case 'source':
      return { markup: tag('source', [[isImagePath(f.path) ? 'srcset' : 'src', url], ['type', mediaType(f.path)]], attrs) + '>' };
    case 'script':
      return { markup: tag('script', [['src', url], ['integrity', integrityOf(f.bytes)]], attrs) + '></script>' };
    case 'stylesheet':
      return { markup: tag('link', [['rel', 'stylesheet'], ['href', url], ['integrity', integrityOf(f.bytes)]], attrs) + '>' };
    case 'modulepreload':
      return { markup: tag('link', [['rel', 'modulepreload'], ['href', url], ['integrity', integrityOf(f.bytes)]], attrs) + '>' };
    case 'preload': {
      const as = attrs.find((a) => a.name.toLowerCase() === 'as')?.value ?? PRELOAD_AS[ext];
      if (as === undefined) throw new SitefileError('E_TYPE', `preload cannot tell what '${f.path}' is; give it as=… (style, script, font, image, fetch, …)`, span);
      const auto: Array<readonly [string, string | undefined]> = [['rel', 'preload'], ['href', url], ['as', as]];
      // Browsers check integrity on preloaded scripts and stylesheets; fonts and fetches load in CORS mode.
      if (as === 'script' || as === 'style') auto.push(['integrity', integrityOf(f.bytes)]);
      if (as === 'font' || as === 'fetch') auto.push(['crossorigin', undefined]);
      return { markup: tag('link', auto, attrs) + '>' };
    }
    case 'pre':
      return { markup: tag('pre', [], attrs) + `>${escapeHtml(decodeText(f, 'pre', span))}</pre>` };
    case 'svg': {
      if (!opts.inlineSvg) {
        throw new SitefileError('E_UNSUPPORTED', `the svg helper pastes '${f.path}' into the page, and SVG markup can run scripts; allow it with RENDER --inline-svg, or use the img helper`, span);
      }
      const text = stripSvgProlog(decodeText(f, 'svg', span));
      if (!/^<svg[\t\n\f\r />]/.test(text)) throw new SitefileError('E_DATA', `'${f.path}' does not start with an <svg> element`, span);
      return { markup: tag('svg', [], attrs) + text.slice(4) };
    }
    default:
      throw new SitefileError('E_TEMPLATE', `unknown file helper '${helper}'`, span);
  }
}

const SVG_WS = /^[\t\n\f\r ]+/;

/** SVG text without a BOM, XML declaration, comments before the root, doctype and surrounding whitespace. */
function stripSvgProlog(s: string): string {
  let t = s.replace(/^﻿/, '').replace(SVG_WS, '');
  for (;;) {
    const end = t.startsWith('<?xml') ? t.indexOf('?>') + 2 : t.startsWith('<!--') ? t.indexOf('-->') + 3 : /^<!DOCTYPE/i.test(t) ? t.indexOf('>') + 1 : 0;
    if (end <= 2) break;
    t = t.slice(end).replace(SVG_WS, '');
  }
  return t.replace(/[\t\n\f\r ]+$/, '');
}

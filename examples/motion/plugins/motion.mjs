// motion.mjs - a Sitefile renderer plugin: text in, animated SVG out.
//
// Runs sandboxed (`node --permission`): imports nothing, reads nothing, writes
// nothing, and is deterministic - the same text and options always produce the
// same bytes. The SVG is self-contained (CSS keyframes only, no scripts, no
// external resources), so it plays in a browser tab and inside <img>.
//
// Options (all optional):
//   style       typewriter | fade | slide | wave | glitch   (default typewriter)
//   duration    seconds each line takes, 0.2-60              (default 2.5)
//   loop        true | false                                 (default true)
//   width,height  pixels, 100-4000                           (default 1200x630)
//   background, color, accent   CSS colors (#hex, name, rgb()/hsl())
//   font        CSS font stack; only [A-Za-z0-9 ,'"-] survive
//   size        maximum font px; shrinks to fit             (default 76)
//   text        used when the TRANSFORM has no file source

export const apiVersion = 1;
// The same request always gives the same bytes, so results may be cached.
export const cache = true;

const STYLES = ['typewriter', 'fade', 'slide', 'wave', 'glitch'];
const KNOWN = ['style', 'duration', 'loop', 'width', 'height', 'background', 'color', 'accent', 'font', 'size', 'text'];
const DEFAULT_FONT = "system-ui, -apple-system, 'Segoe UI', Helvetica, Arial, sans-serif";
// wave places each letter in a fixed cell (ADVANCE x size): a monospace face fills
// those cells evenly, so it is wave's default.
const MONO_FONT = "ui-monospace, 'SF Mono', Menlo, Consolas, 'DejaVu Sans Mono', monospace";
const COLOR = /^(?:#(?:[0-9a-f]{3,4}|[0-9a-f]{6}|[0-9a-f]{8})|[a-z]{3,24}|(?:rgb|hsl)a?\([0-9 ,.%/]{3,40}\))$/i;
const ADVANCE = 0.6; // average glyph width as a fraction of font size, used to fit text

function fail(message) {
  throw new Error(`motion plugin: ${message}`);
}

// ---------------------------------------------------------------- options

function number(options, key, fallback, min, max, integer = false) {
  const raw = options[key];
  if (raw === undefined || raw === '') return fallback;
  const n = Number(raw);
  if (!Number.isFinite(n) || n < min || n > max || (integer && !Number.isInteger(n))) {
    fail(`option "${key}" must be ${integer ? 'an integer' : 'a number'} from ${min} to ${max}, got "${raw}"`);
  }
  return n;
}

function color(options, key, fallback) {
  const raw = options[key];
  if (raw === undefined || raw === '') return fallback;
  if (!COLOR.test(raw)) fail(`option "${key}" must be a CSS color (#hex, name, rgb() or hsl()), got "${raw}"`);
  return raw;
}

function parseOptions(options) {
  for (const key of Object.keys(options)) {
    if (!KNOWN.includes(key)) fail(`unknown option "${key}" (known: ${KNOWN.join(', ')})`);
  }
  const style = options.style || 'typewriter';
  if (!STYLES.includes(style)) fail(`option "style" must be one of ${STYLES.join(', ')}, got "${style}"`);
  const loop = options.loop === undefined || options.loop === '' ? 'true' : options.loop;
  if (loop !== 'true' && loop !== 'false') fail(`option "loop" must be true or false, got "${loop}"`);
  const font =
    (options.font ?? '').replace(/[^A-Za-z0-9 ,'"-]/g, '').replace(/ +/g, ' ').trim() || (style === 'wave' ? MONO_FONT : DEFAULT_FONT);
  return {
    style,
    loop: loop === 'true',
    duration: number(options, 'duration', 2.5, 0.2, 60),
    width: number(options, 'width', 1200, 100, 4000, true),
    height: number(options, 'height', 630, 100, 4000, true),
    size: number(options, 'size', 76, 8, 1000),
    background: color(options, 'background', '#0b1020'),
    color: color(options, 'color', '#f5f7ff'),
    accent: color(options, 'accent', '#5eead4'),
    font,
  };
}

function pickText(request) {
  const title = (v) => (v && typeof v === 'object' && ['string', 'number'].includes(typeof v.title) ? String(v.title) : undefined);
  const text = request.input?.text ?? request.options?.text ?? title(request.item) ?? title(request.data)
    ?? (typeof request.item === 'string' ? request.item : undefined);
  const lines = String(text ?? '').split(/\r?\n/).map((l) => l.trim()).filter(Boolean);
  if (lines.length === 0) fail('no text: give the TRANSFORM a source file, OPTIONS text=..., or an item/data with a title');
  if (lines.length > 20) fail(`too many lines (${lines.length}); the limit is 20`);
  return lines;
}

// ---------------------------------------------------------------- helpers

// Escape for XML text and attributes; drop characters XML 1.0 forbids.
function esc(s) {
  return s
    .replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f￾￿]/g, '')
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;').replace(/'/g, '&apos;');
}

const r3 = (n) => String(Math.round(n * 1000) / 1000);

// ---------------------------------------------------------------- render

export function transform(request) {
  const o = parseOptions(request.options ?? {});
  const lines = pickText(request);
  const n = lines.length;
  const { width: W, height: H, duration: d } = o;
  const chars = lines.map((l) => Array.from(l));

  // Layout: left-aligned block, vertically centered, shrunk until it fits.
  const pad = Math.round(Math.min(W, H) * 0.1);
  const longest = Math.max(...chars.map((c) => c.length));
  const fs = Math.max(8, Math.floor(Math.min(
    o.size, (W - 2 * pad) / (ADVANCE * longest), (H - 2 * pad) / (n * 1.3),
  )));
  const lh = fs * 1.3;
  const top = (H - n * lh) / 2;
  const baseline = (i) => top + lh * (i + 0.5) + fs * 0.35;

  // Timeline: line i starts at i*d; after the last line the cycle holds, then restarts.
  const hold = o.loop ? 1.6 : 0;
  const T = n * d + hold;
  const pc = (t) => `${r3((t / T) * 100)}%`;
  const frames = (name, stops) =>
    `@keyframes ${name}{${stops.map(([t, css]) => `${pc(t)}{${css}}`).join('')}}`;

  // Each line gets keyframes: hidden until its start, revealed, held, faded at cycle end.
  const css = [];
  const lineStops = (i, hidden, shown, reveal, ease = '') => {
    const s = i * d;
    const stops = [[0, hidden], [s, hidden + ease], [s + reveal, shown]];
    if (o.loop) stops.push([T - 0.4, shown], [T, hidden]);
    return stops;
  };
  for (let i = 0; i < n; i++) {
    const s = i * d;
    let stops;
    switch (o.style) {
      case 'typewriter': // clip the line open from the left, one glyph per step
        stops = lineStops(i, 'clip-path:inset(-20% 100% -20% 0)', 'clip-path:inset(-20% 0 -20% 0)', d * 0.75,
          `;animation-timing-function:steps(${chars[i].length},end)`);
        break;
      case 'fade':
        stops = lineStops(i, `opacity:0;transform:translateY(${r3(fs * 0.3)}px)`, 'opacity:1;transform:translateY(0)', d * 0.7,
          ';animation-timing-function:ease-out');
        break;
      case 'slide':
        stops = lineStops(i, `opacity:0;transform:translateX(${r3(-W * 0.12)}px)`, 'opacity:1;transform:translateX(0)', d * 0.6,
          ';animation-timing-function:cubic-bezier(.2,.9,.2,1)');
        break;
      case 'wave': // letters are revealed by a quick fade, then bounce on their own clocks
        stops = lineStops(i, 'opacity:0', 'opacity:1', Math.min(0.4, d * 0.3));
        break;
      default: { // glitch: flicker on, like a bad signal
        const f = Math.min(0.1, d * 0.05);
        stops = [[0, 'opacity:0'], [s, 'opacity:0'], [s + f, 'opacity:1'], [s + 2 * f, 'opacity:.25'], [s + 3 * f, 'opacity:1']];
        if (o.loop) stops.push([T - 0.4, 'opacity:1'], [T, 'opacity:0']);
      }
    }
    css.push(frames(`k${i}`, stops), `.l${i}{animation-name:k${i}}`);
  }

  if (o.style === 'wave') {
    css.push('@keyframes bounce{0%,60%,100%{transform:translateY(0);fill:var(--fg)}' +
      `30%{transform:translateY(${r3(-fs * 0.22)}px);fill:var(--ac)}}`,
    `.c{transform-box:fill-box;transform-origin:center;animation:bounce 1.4s ease-in-out infinite both;` +
      `${o.loop ? '' : 'animation-iteration-count:2;'}}`);
  }
  if (o.style === 'glitch') {
    const burst = (dx, color) =>
      `0%,84%,100%{opacity:0}86%{opacity:.9;transform:translate(${dx}px,2px);clip-path:inset(10% 0 55% 0)}` +
      `91%{opacity:.9;transform:translate(${-dx}px,-2px);clip-path:inset(60% 0 8% 0);fill:${color}}` +
      `96%{opacity:.9;transform:translate(${dx / 2}px,0);clip-path:inset(30% 0 40% 0)}`;
    css.push(`@keyframes g1{${burst(-7, 'var(--ac)')}}`, `@keyframes g2{${burst(7, '#ff3d81')}}`,
      '.g1,.g2{opacity:0;mix-blend-mode:screen;animation:2.2s steps(1,end) infinite}',
      '.g1{animation-name:g1;fill:var(--ac)}.g2{animation-name:g2;fill:#ff3d81}');
  }

  // Body: one <g> per line, with style-specific children.
  const body = lines.map((line, i) => {
    const y = r3(baseline(i));
    let inner;
    if (o.style === 'wave') {
      const cell = fs * ADVANCE;
      const x0 = pad + cell / 2;
      inner = chars[i].map((ch, k) => (ch === ' ' ? '' :
        `<text class="c" x="${r3(x0 + k * cell)}" y="${y}" style="animation-delay:${r3(i * d + k * 0.07)}s">${esc(ch)}</text>`)).join('');
    } else if (o.style === 'glitch') {
      const t = (cls) => `<text class="${cls}" x="${pad}" y="${y}" style="animation-delay:${r3(i * d)}s">${esc(line)}</text>`;
      inner = `<text x="${pad}" y="${y}">${esc(line)}</text>${t('g1 gl')}${t('g2 gl')}`;
    } else {
      inner = `<text x="${pad}" y="${y}">${esc(line)}</text>`;
    }
    return `<g class="l l${i}">${inner}</g>`;
  }).join('\n  ');

  const style = [
    `:root{--fg:${o.color};--ac:${o.accent}}`,
    `text{font:700 ${fs}px ${o.font};letter-spacing:-.01em;fill:var(--fg)}`,
    `.l{animation-duration:${r3(T)}s;animation-iteration-count:${o.loop ? 'infinite' : '1'};animation-fill-mode:both;animation-timing-function:linear}`,
    `.bar{transform-box:fill-box;transform-origin:left;animation:bar ${r3(T)}s linear ${o.loop ? 'infinite' : 'forwards'}}`,
    '@keyframes bar{from{transform:scaleX(0)}to{transform:scaleX(1)}}',
    ...css,
    '@media (prefers-reduced-motion:reduce){*{animation:none!important}.bar,.gl{display:none}}',
  ].join('\n');

  const label = esc(lines.join(' / '));
  const svg = `<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" role="img" aria-labelledby="t" width="${W}" height="${H}" viewBox="0 0 ${W} ${H}">
  <title id="t">${label}</title>
  <defs><radialGradient id="glow" cx="85%" cy="0%" r="90%"><stop offset="0" stop-color="${esc(o.accent)}" stop-opacity=".28"/><stop offset="1" stop-color="${esc(o.accent)}" stop-opacity="0"/></radialGradient></defs>
  <style>
${style}
  </style>
  <rect width="${W}" height="${H}" fill="${esc(o.background)}"/>
  <rect width="${W}" height="${H}" fill="url(#glow)"/>
  ${body}
  <rect class="bar" x="0" y="${H - 6}" width="${W}" height="6" fill="${esc(o.accent)}"/>
</svg>
`;
  return { bytes: svg, mediaType: 'image/svg+xml' };
}

export default { apiVersion, transform };

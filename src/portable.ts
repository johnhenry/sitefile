// Path portability (docs/LANGUAGE.md, "Portable paths"): names that work locally but break on other
// systems or hosts. Ported as rust/src/portable.rs: both must find the same problems, in the same words.

/** Windows device names: a file named so (with any extension) cannot be created there. */
const RESERVED = new Set(['CON', 'PRN', 'AUX', 'NUL', ...[1, 2, 3, 4, 5, 6, 7, 8, 9, '¹', '²', '³'].flatMap((n) => [`COM${n}`, `LPT${n}`])]);
/** Characters Windows does not allow in a file name. */
const WINDOWS_CHARS = /[<>:"|?*\\\u0000-\u001f]/;

/** Why a path cannot be written on Windows, or undefined. */
export function windowsProblem(path: string): string | undefined {
  for (const seg of path.split('/').slice(1)) {
    const bad = WINDOWS_CHARS.exec(seg);
    if (bad) {
      const c = bad[0]!;
      const shown = c.charCodeAt(0) < 0x20 ? `U+${c.charCodeAt(0).toString(16).toUpperCase().padStart(4, '0')}` : `'${c}'`;
      return `${shown} is not allowed in a Windows file name`;
    }
    if (seg.endsWith('.') || seg.endsWith(' ')) return `'${seg}' ends with ${seg.endsWith('.') ? 'a dot' : 'a space'}, which Windows drops`;
    const stem = seg.split('.')[0]!.replace(/ +$/, '').toUpperCase();
    if (RESERVED.has(stem)) return `'${seg}' is a reserved device name on Windows`;
  }
  return undefined;
}

/** Root files Netlify and Cloudflare Pages read; GitHub Pages' Jekyll skipping them does no harm. */
const HOST_FILES = new Set(['/_headers', '/_redirects']);

/**
 * Portability problems of the exported files (`paths` are below the export root, in UTF-8 order): one message
 * per path Windows cannot hold, then one about GitHub Pages when a file or folder starts with `_` (other than
 * `/_headers` and `/_redirects`) and there is no `/.nojekyll`.
 */
export function portabilityProblems(paths: readonly string[]): string[] {
  const out: string[] = [];
  for (const p of paths) {
    const why = windowsProblem(p);
    if (why) out.push(`'${p}' cannot be extracted on Windows: ${why}`);
  }
  if (!paths.includes('/.nojekyll')) {
    const hidden = paths.filter((p) => !HOST_FILES.has(p) && p.split('/').some((s) => s.startsWith('_')));
    if (hidden.length > 0) {
      const shown = hidden.slice(0, 3).join(', ') + (hidden.length > 3 ? `, … (${hidden.length} in all)` : '');
      out.push(`GitHub Pages (with Jekyll) does not publish files or folders whose name starts with '_' (${shown}); if the site is hosted there, add an empty /.nojekyll`);
    }
  }
  return out;
}

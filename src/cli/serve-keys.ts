import { spawn } from 'node:child_process';
import { encodeQr, qrTerminal } from '../qr.js';
import { formatUrl, networkUrls } from '../server.js';

/**
 * `serve` in a terminal (issue #224): single keys while serving in the foreground, `--open[=path]` and
 * `--qr`. Ported as rust/src/cli/serve_keys.rs (the same keys, messages and QR codes).
 */
export type KeyAction = 'open' | 'urls' | 'restart' | 'clear' | 'quit' | 'help';

/** What a key does (letters in either case; Ctrl-C and Ctrl-D quit), or undefined. */
export function keyAction(byte: number): KeyAction | undefined {
  if (byte === 3 || byte === 4) return 'quit';
  switch (String.fromCharCode(byte).toLowerCase()) {
    case 'o':
      return 'open';
    case 'u':
      return 'urls';
    case 'r':
      return 'restart';
    case 'c':
      return 'clear';
    case 'q':
      return 'quit';
    case 'h':
      return 'help';
    default:
      return undefined;
  }
}

export const KEYS_HINT = 'Press h for keys\n';
export const KEYS_HELP = 'Keys: o open in the browser, u show the URLs, r rebuild (--watch) or re-read the directory, c clear the screen, q quit, h this help\n';
export const CLEAR = '\x1b[2J\x1b[3J\x1b[H';

const isWildcard = (h: string): boolean => h === '0.0.0.0' || h === '::';

/**
 * The URLs to show (below `serve --base`, `https://` for a TLS server): the local one, and when bound to every
 * interface the same network URLs as the start line (one per interface address, IPv4 first).
 */
export function serveUrls(host: string, port: number, base = '/', scheme: 'http' | 'https' = 'http'): { local: string; network: string[] } {
  const path = base.slice(1);
  const local = formatUrl(host, port, scheme) + path;
  return { local, network: isWildcard(host) ? networkUrls(host, port, scheme).map((u) => u + path) : [] };
}

/** `u`: the URLs, and a QR code of the first network URL when there is one. */
export function urlsText(urls: { local: string; network: readonly string[] }): string {
  let out = `  Local:   ${urls.local}\n`;
  for (const n of urls.network) out += `  Network: ${n}\n`;
  if (urls.network[0] !== undefined) out += qrText(urls.network[0]);
  return out;
}

/** `--qr`: the URL and its QR code. */
export function qrText(url: string): string {
  const code = encodeQr(url);
  return code ? qrTerminal(code) : '';
}

/** `--open=<path>` (or `--open`, `/`) joined to the server's URL. */
export function openUrl(base: string, path: string): string {
  return base + path.replace(/^\/+/, '');
}

/**
 * Open `url` in the browser: `$BROWSER` when set (a program, run with the URL as its only argument, no
 * shell), else `open` (macOS), `rundll32 url.dll,FileProtocolHandler` (Windows) or `xdg-open`. A failure is
 * a warning: the server keeps running.
 */
export function openBrowser(url: string, warn: (m: string) => void): void {
  const browser = process.env['BROWSER'];
  const [cmd, args]: [string, string[]] =
    browser !== undefined && browser !== ''
      ? [browser, [url]]
      : process.platform === 'darwin'
        ? ['open', [url]]
        : process.platform === 'win32'
          ? ['rundll32', ['url.dll,FileProtocolHandler', url]]
          : ['xdg-open', [url]];
  try {
    const child = spawn(cmd, args, { stdio: 'ignore', detached: true });
    child.on('error', (e) => warn(`cannot open the browser (${cmd}): ${e.message}`));
    child.unref();
  } catch (e) {
    warn(`cannot open the browser (${cmd}): ${(e as Error).message}`);
  }
}

/**
 * Read single keys from stdin while serving: when stdin is a terminal (raw mode), or when
 * `SITEFILE_SERVE_KEYS=1` (tests drive it through a pipe). Returns a function that stops reading and
 * restores the terminal, or undefined when keys are off.
 */
export function readKeys(onAction: (a: KeyAction) => void): (() => void) | undefined {
  const tty = !!process.stdin.isTTY;
  if (!tty && process.env['SITEFILE_SERVE_KEYS'] !== '1') return undefined;
  if (tty) process.stdin.setRawMode(true);
  const onData = (chunk: Buffer): void => {
    for (const b of chunk) {
      const a = keyAction(b);
      if (a) onAction(a);
    }
  };
  process.stdin.on('data', onData);
  process.stdin.resume();
  return () => {
    process.stdin.off('data', onData);
    if (tty) process.stdin.setRawMode(false);
    process.stdin.pause();
  };
}

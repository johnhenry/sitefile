import { spawn } from 'node:child_process';
import { createSocket } from 'node:dgram';
import { encodeQr, qrTerminal } from '../qr.js';
import { formatUrl } from '../server.js';

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
 * This machine's address on the network: the local address of a UDP socket "connected" to a public
 * address (nothing is sent), or undefined without a route. IPv4 for 0.0.0.0, IPv6 for ::.
 */
export async function networkAddress(bindHost: string): Promise<string | undefined> {
  const v6 = bindHost === '::';
  return new Promise((resolve) => {
    const s = createSocket(v6 ? 'udp6' : 'udp4');
    const done = (a: string | undefined): void => {
      s.close();
      resolve(a);
    };
    s.on('error', () => done(undefined));
    s.connect(9, v6 ? '2001:db8::1' : '192.0.2.1', () => {
      try {
        const a = s.address().address;
        done(a === '0.0.0.0' || a === '::' ? undefined : a);
      } catch {
        done(undefined);
      }
    });
  });
}

/** The URLs to show (below `serve --base`): the local one, and the network one when bound to every interface. */
export async function serveUrls(host: string, port: number, base = '/'): Promise<{ local: string; network: string | undefined }> {
  const path = base.slice(1);
  const local = formatUrl(host, port) + path;
  if (!isWildcard(host)) return { local, network: undefined };
  const a = await networkAddress(host);
  return { local, network: a === undefined ? undefined : formatUrl(a, port) + path };
}

/** `u`: the URLs, and a QR code of the network URL when there is one. */
export function urlsText(urls: { local: string; network: string | undefined }): string {
  let out = `  Local:   ${urls.local}\n`;
  if (urls.network !== undefined) out += `  Network: ${urls.network}\n${qrText(urls.network)}`;
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

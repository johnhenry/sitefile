import { SitefileError } from '../errors.js';
import { mergeNetwork, networkFromEnv, networkProblem, parseNetworkMode, splitList, type NetworkSettings } from '../resources/network.js';

/** parseArgs options for the network flags (build, validate, inspect, graph, why, pull, push, vendor, outdated). */
export const NETWORK_OPTIONS = {
  proxy: { type: 'string' },
  'no-proxy': { type: 'string', multiple: true },
  'allow-hosts': { type: 'string', multiple: true },
  'deny-hosts': { type: 'string', multiple: true },
  network: { type: 'string' },
  'allow-secret-host': { type: 'string', multiple: true },
  'connect-timeout': { type: 'string' },
  'read-timeout': { type: 'string' },
} as const;

/** Help lines for {@link NETWORK_OPTIONS}, at the given flag-column width. */
export function networkHelp(width: number): string[] {
  const rows: Array<[string, string[]]> = [
    ['--proxy <url>', ['send requests through this HTTP proxy: http://[user:password@]host[:port]', '(also $SITEFILE_PROXY; HTTP(S)_PROXY are never read)']],
    ['--no-proxy <hosts>', ['hosts reached directly, not through the proxy (NO_PROXY syntax; repeatable,', 'comma-separated; also $SITEFILE_NO_PROXY)']],
    ['--allow-hosts <hosts>', ['contact only hosts matching these patterns (repeatable, comma-separated;', 'also $SITEFILE_ALLOW_HOSTS)']],
    ['--deny-hosts <hosts>', ['never contact hosts matching these patterns; beats --allow-hosts', '(repeatable, comma-separated; also $SITEFILE_DENY_HOSTS)']],
    ['--network <mode>', ['open (default) or strict: every host must match --allow-hosts, even with none', 'given (also $SITEFILE_NETWORK)']],
    ['--allow-secret-host <id=host>', ['in strict mode, let IMPORT ... SECRET <id> send its secret to this host', '(a host pattern; repeatable)']],
    ['--connect-timeout <ms>', ['time limit for opening each connection (also $SITEFILE_FETCH_TIMEOUT)']],
    ['--read-timeout <ms>', ['longest wait for data on a connection (also $SITEFILE_FETCH_TIMEOUT)']],
  ];
  const out: string[] = [];
  for (const [flag, lines] of rows) {
    // A flag wider than the column gets a line of its own.
    if (flag.length >= width) out.push(flag);
    lines.forEach((l, i) => out.push(`${(i === 0 && flag.length < width ? flag : '').padEnd(width)}${l}`));
  }
  return out;
}

function ms(raw: string | undefined, flag: string): number | undefined {
  if (raw === undefined) return undefined;
  const n = Number(raw);
  if (!/^\d+$/.test(raw) || !Number.isSafeInteger(n) || n <= 0) throw new SitefileError('E_USAGE', `${flag} expects a positive integer`);
  return n;
}

type Values = Record<string, string | boolean | Array<string | boolean> | undefined>;

const strings = (v: unknown): string[] => (Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string') : []);

/**
 * The network settings for a command: the SITEFILE_* environment, then the flags on top (flags replace
 * the proxy and timeouts and add to the host lists). Throws E_USAGE for a bad value.
 */
export function networkOption(v: Values, env: NodeJS.ProcessEnv = process.env): NetworkSettings | undefined {
  const flags: NetworkSettings = {
    ...(typeof v.proxy === 'string' ? { proxy: v.proxy } : {}),
    noProxy: strings(v['no-proxy']).flatMap(splitList),
    allowHosts: strings(v['allow-hosts']).flatMap(splitList),
    denyHosts: strings(v['deny-hosts']).flatMap(splitList),
    secretHosts: strings(v['allow-secret-host']).flatMap(splitList),
    connectTimeoutMs: ms(typeof v['connect-timeout'] === 'string' ? v['connect-timeout'] : undefined, '--connect-timeout'),
    readTimeoutMs: ms(typeof v['read-timeout'] === 'string' ? v['read-timeout'] : undefined, '--read-timeout'),
  };
  const fromEnv = networkFromEnv(env);
  // --network replaces SITEFILE_NETWORK (--network open turns an inherited strict mode off).
  const mode = typeof v.network === 'string' ? (parseNetworkMode(v.network === '' ? '?' : v.network, '--network') ?? false) : undefined;
  const merged = mergeNetwork(mode === undefined ? fromEnv : { ...fromEnv, strict: undefined }, mode ? { ...flags, strict: true } : flags);
  const problem = networkProblem(merged);
  if (problem) throw new SitefileError('E_USAGE', problem);
  return Object.keys(merged).length ? merged : undefined;
}

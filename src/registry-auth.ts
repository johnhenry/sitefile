import { execFile } from 'node:child_process';
import { promises as fs } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { SitefileError } from './errors.js';

/**
 * Registry credentials. Sources, first match wins:
 *
 *   1. explicit  (`--username` + `--password-stdin`; applies only to the registry the command names)
 *   2. $SITEFILE_REGISTRY_AUTH   JSON: { "<host>": "user:password" | {"username","password"} | {"identitytoken"} }
 *   3. Docker config ($DOCKER_CONFIG/config.json or ~/.docker/config.json):
 *        credHelpers[<host>]            -> docker-credential-<name> get
 *        auths[<host>].auth|username/password|identitytoken
 *        credsStore                     -> docker-credential-<name> get
 *
 * Running a credential helper executes a program, so it happens only when the config names one
 * (the name must be a plain word, it is run without a shell, the registry URL goes to its stdin, and
 * nothing secret is ever on argv). Secrets are returned to the caller and never logged or put in an
 * error message.
 */
export interface Credentials {
  readonly username?: string;
  readonly password?: string;
  /** A Docker "identity token" (OAuth2 refresh token), exchanged at the token endpoint. */
  readonly identityToken?: string;
}

export interface CredentialOptions {
  readonly explicit?: Credentials | undefined;
  readonly env?: NodeJS.ProcessEnv;
  /** Helper timeout (ms). */
  readonly helperTimeoutMs?: number;
}

const DOCKER_HUB_KEYS = ['docker.io', 'index.docker.io', 'registry-1.docker.io'];

/** `https://index.docker.io/v1/`, `registry.example.com:5000/x` -> host key. */
function hostKey(k: string): string {
  const noScheme = k.replace(/^[a-z]+:\/\//i, '');
  const host = (noScheme.split('/')[0] ?? '').toLowerCase();
  return DOCKER_HUB_KEYS.includes(host) ? 'docker.io' : host;
}

/** Names under which a registry may appear in a docker config. */
function matches(configKey: string, registry: string): boolean {
  return hostKey(configKey) === hostKey(registry);
}

export function dockerConfigPath(env: NodeJS.ProcessEnv): string {
  const dir = env.DOCKER_CONFIG;
  return path.join(dir !== undefined && dir !== '' ? dir : path.join(os.homedir(), '.docker'), 'config.json');
}

function isRecord(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

function str(v: unknown): string | undefined {
  return typeof v === 'string' && v !== '' ? v : undefined;
}

function fromEntry(e: unknown): Credentials | undefined {
  if (typeof e === 'string') {
    const i = e.indexOf(':');
    return i > 0 ? { username: e.slice(0, i), password: e.slice(i + 1) } : undefined;
  }
  if (!isRecord(e)) return undefined;
  const auth = str(e.auth);
  if (auth) {
    const text = Buffer.from(auth, 'base64').toString('utf8');
    const i = text.indexOf(':');
    if (i > 0) return { username: text.slice(0, i), password: text.slice(i + 1) };
  }
  const identityToken = str(e.identitytoken) ?? str(e.identityToken);
  if (identityToken) return { identityToken, ...(str(e.username) ? { username: str(e.username) as string } : {}) };
  const username = str(e.username);
  const password = str(e.password);
  if (username && password) return { username, password };
  return undefined;
}

/** Docker's `docker-credential-<name> get`: the server URL on stdin, JSON on stdout. */
export function runCredentialHelper(name: string, serverUrl: string, env: NodeJS.ProcessEnv, timeoutMs: number): Promise<Credentials | undefined> {
  if (!/^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/.test(name)) {
    throw new SitefileError('E_AUTH', `refusing to run credential helper with unusual name '${name.slice(0, 40)}'`);
  }
  return new Promise((resolve, reject) => {
    const child = execFile(
      `docker-credential-${name}`,
      ['get'],
      { env, timeout: timeoutMs, maxBuffer: 1 << 20, windowsHide: true },
      (err, stdout) => {
        if (err) {
          // The helper protocol's "no entry for this server" answer means anonymous access.
          if (typeof stdout === 'string' && /credentials not found/i.test(stdout)) {
            resolve(undefined);
            return;
          }
          // Deliberately no stdout/stderr: it may contain the secret.
          const code = (err as NodeJS.ErrnoException).code;
          reject(
            new SitefileError(
              'E_AUTH',
              code === 'ENOENT'
                ? `credential helper docker-credential-${name} (named in your Docker config) was not found on PATH`
                : `credential helper docker-credential-${name} failed`,
            ),
          );
          return;
        }
        try {
          const j = JSON.parse(stdout) as { Username?: unknown; Secret?: unknown };
          const user = str(j.Username);
          const secret = str(j.Secret);
          if (user === '<token>' && secret) resolve({ identityToken: secret });
          else resolve(user && secret ? { username: user, password: secret } : undefined);
        } catch {
          reject(new SitefileError('E_AUTH', `credential helper docker-credential-${name} returned unreadable output`));
        }
      },
    );
    child.stdin?.on('error', () => undefined);
    child.stdin?.end(serverUrl);
  });
}

async function readJson(file: string): Promise<unknown> {
  let text: string;
  try {
    text = await fs.readFile(file, 'utf8');
  } catch (e) {
    if ((e as NodeJS.ErrnoException).code === 'ENOENT') return undefined;
    throw new SitefileError('E_AUTH', `cannot read ${file}: ${(e as Error).message}`);
  }
  try {
    return JSON.parse(text);
  } catch {
    throw new SitefileError('E_AUTH', `${file} is not valid JSON`);
  }
}

/** Resolve credentials for `registry` (canonical host, e.g. `docker.io` or `localhost:5000`), or undefined for anonymous access. */
export async function resolveCredentials(registry: string, options: CredentialOptions = {}): Promise<Credentials | undefined> {
  if (options.explicit) return options.explicit;
  const env = options.env ?? process.env;

  const raw = env.SITEFILE_REGISTRY_AUTH;
  if (raw !== undefined && raw !== '') {
    let j: unknown;
    try {
      j = JSON.parse(raw);
    } catch {
      throw new SitefileError('E_AUTH', 'SITEFILE_REGISTRY_AUTH must be a JSON object mapping registry hosts to "user:password"');
    }
    if (!isRecord(j)) throw new SitefileError('E_AUTH', 'SITEFILE_REGISTRY_AUTH must be a JSON object mapping registry hosts to "user:password"');
    for (const [k, v] of Object.entries(j)) {
      if (matches(k, registry)) {
        const c = fromEntry(v);
        if (c) return c;
      }
    }
  }

  const cfg = await readJson(dockerConfigPath(env));
  if (!isRecord(cfg)) return undefined;
  const timeout = options.helperTimeoutMs ?? 15_000;
  const serverUrl = hostKey(registry) === 'docker.io' ? 'https://index.docker.io/v1/' : hostKey(registry);

  const helpers = isRecord(cfg.credHelpers) ? cfg.credHelpers : {};
  for (const [k, v] of Object.entries(helpers)) {
    if (matches(k, registry) && str(v)) return runCredentialHelper(str(v) as string, serverUrl, env, timeout);
  }
  const auths = isRecord(cfg.auths) ? cfg.auths : {};
  for (const [k, v] of Object.entries(auths)) {
    if (matches(k, registry)) {
      const c = fromEntry(v);
      if (c) return c;
    }
  }
  const store = str(cfg.credsStore);
  if (store) return runCredentialHelper(store, serverUrl, env, timeout);
  return undefined;
}

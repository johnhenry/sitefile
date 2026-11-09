import { readFile } from 'node:fs/promises';
import path from 'node:path';
import { SitefileError } from './errors.js';
import { parseYaml } from './yaml.js';

/**
 * The servers file (issue #223), for `sitefile up`, `down` and `ps`: several detached site servers
 * described in one place, like a Compose file.
 *
 * ```yaml
 * servers:
 *   docs:
 *     source: ./docs/dist     # a directory or archive file (relative to this file), or an image
 *     publish: 8081           # [host:]port, as serve -p (default 127.0.0.1, a free port)
 *     spa: true               # as serve --spa (default false)
 * ```
 *
 * Each key under `servers` is the server's name (as `serve -d --name`).
 */
export const SERVERS_FILES: readonly string[] = ['sitefile.servers.yaml', 'sitefile.servers.yml', 'sitefile.servers.json'];

export interface ServerSpec {
  readonly name: string;
  /** As written: `./dir`, `../site.zip`, `/abs`, an image reference or ID, or an archive file name. */
  readonly source: string;
  /** `[host:]port` as written (a number is its decimal form); undefined: 127.0.0.1 and a free port. */
  readonly publish: string | undefined;
  readonly spa: boolean;
}

export interface ServersFile {
  /** The file as given (for messages). */
  readonly label: string;
  /** The directory relative sources are resolved against. */
  readonly dir: string;
  readonly servers: readonly ServerSpec[];
}

const NAME_RE = /^[A-Za-z0-9][A-Za-z0-9_.-]{0,62}$/;
const SERVER_KEYS = ['source', 'publish', 'spa'] as const;

const isMapping = (v: unknown): v is Record<string, unknown> => v !== null && typeof v === 'object' && !Array.isArray(v);

function fail(label: string, detail: string): never {
  throw new SitefileError('E_DATA', `${label}: ${detail}`);
}

/** `file` when given, else the first of {@link SERVERS_FILES} in the current directory. */
export async function findServersFile(file: string | undefined): Promise<string> {
  if (file !== undefined) return file;
  for (const name of SERVERS_FILES) {
    if (await readFile(name).then(() => true, () => false)) return name;
  }
  throw new SitefileError('E_NOT_FOUND', `no servers file: ${SERVERS_FILES.join(', ')} not found in the current directory (or pass -f)`);
}

/** Check the shape of a parsed servers file. */
export function parseServersDefinition(value: unknown, label: string, dir: string): ServersFile {
  if (!isMapping(value)) fail(label, 'a servers file must be a mapping with a servers key');
  for (const key of Object.keys(value)) if (key !== 'servers') fail(label, `unknown key '${key}' (expected servers)`);
  const raw = value['servers'];
  if (!isMapping(raw) || Object.keys(raw).length === 0) fail(label, 'servers must be a non-empty mapping of name: server');
  const servers: ServerSpec[] = [];
  for (const [name, s] of Object.entries(raw)) {
    if (!NAME_RE.test(name)) fail(label, `'${name}' is not a valid server name (letters, digits, '_', '.', '-'; at most 63)`);
    // A key of digits only would be reordered by JavaScript objects; file order is the start order.
    if (/^[0-9]+$/.test(name)) fail(label, `'${name}' is not a valid server name here: use at least one letter`);
    const where = `server '${name}'`;
    if (!isMapping(s)) fail(label, `${where} must be a mapping with a source`);
    for (const key of Object.keys(s)) {
      if (!(SERVER_KEYS as readonly string[]).includes(key)) fail(label, `${where}: unknown key '${key}' (expected ${SERVER_KEYS.join(', ')})`);
    }
    const source = s['source'];
    if (typeof source !== 'string' || source === '') fail(label, `${where}: source must be a non-empty string`);
    const publish = s['publish'];
    if (publish !== undefined && publish !== null && typeof publish !== 'string' && !(typeof publish === 'number' && Number.isSafeInteger(publish) && publish >= 0)) {
      fail(label, `${where}: publish must be [host:]port (a string or a port number)`);
    }
    const spa = s['spa'];
    if (spa !== undefined && spa !== null && typeof spa !== 'boolean') fail(label, `${where}: spa must be true or false`);
    servers.push({ name, source, publish: publish === undefined || publish === null ? undefined : String(publish), spa: spa === true });
  }
  return { label, dir, servers };
}

/** Read and check a servers file (.yaml, .yml or .json). */
export async function loadServersFile(file: string): Promise<ServersFile> {
  const base = path.basename(file).toLowerCase();
  const json = base.endsWith('.json');
  if (!json && !base.endsWith('.yaml') && !base.endsWith('.yml')) {
    throw new SitefileError('E_USAGE', `servers file '${file}': unknown format (use a .yaml, .yml or .json file)`);
  }
  let text: string;
  try {
    text = await readFile(file, 'utf8');
  } catch (e) {
    throw new SitefileError('E_NOT_FOUND', `cannot read servers file '${file}'`, undefined, { cause: e });
  }
  let value: unknown;
  if (!json) value = parseYaml(text, file);
  else {
    try {
      value = JSON.parse(text.replace(/^﻿/, '')) as unknown;
    } catch (e) {
      throw new SitefileError('E_DATA', `${file} is not valid JSON: ${(e as Error).message}`, undefined, { cause: e });
    }
  }
  return parseServersDefinition(value, file, path.dirname(path.resolve(file)));
}

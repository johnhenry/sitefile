import { NETWORK_OPTIONS, networkHelp, networkOption } from './network.js';
import { insecureRegistriesFromEnv, makeClient } from '../base-image.js';
import { SitefileError } from '../errors.js';
import { parseRef, refKey, withDefaultRegistry, canonicalRef } from '../ref.js';
import type { Credentials } from '../registry-auth.js';
import { pullImage, pushImage } from '../registry-ops.js';
import type { RegistryOptions } from '../registry.js';
import { shortId, Store } from '../store.js';
import { requireArgs, str, strs, usage, type Command, type ParsedValues } from './common.js';

async function readStdin(): Promise<string> {
  const chunks: Buffer[] = [];
  let n = 0;
  for await (const c of process.stdin as AsyncIterable<Buffer>) {
    n += c.length;
    if (n > 64 * 1024) usage('--password-stdin: input is too long');
    chunks.push(c);
  }
  return Buffer.concat(chunks).toString('utf8').replace(/\r?\n$/, '');
}

function positive(raw: string | undefined, flag: string): number | undefined {
  if (raw === undefined) return undefined;
  const n = Number(raw);
  if (!Number.isSafeInteger(n) || n <= 0) usage(`${flag} expects a positive integer`);
  return n;
}

const COMMON_FLAGS = [
  '--insecure-registry <host[:port]>   allow plain http to this registry (repeatable; localhost always may;',
  '                                    also $SITEFILE_INSECURE_REGISTRIES, comma-separated)',
  '--username <name>                   registry user (with --password-stdin); otherwise credentials come from',
  '                                    $SITEFILE_REGISTRY_AUTH or ~/.docker/config.json (auths, credHelpers, credsStore)',
  '--password-stdin                    read the password from stdin',
  '--allow-private-network             let redirects/token servers on other hosts resolve to private addresses',
  '--timeout <ms>                      per-request idle timeout (default 30000; --read-timeout wins)',
  ...networkHelp(36),
];

const COMMON_OPTIONS = {
  ...NETWORK_OPTIONS,
  'insecure-registry': { type: 'string', multiple: true },
  username: { type: 'string' },
  'password-stdin': { type: 'boolean' },
  'allow-private-network': { type: 'boolean' },
  timeout: { type: 'string' },
} as const;

async function clientOptions(v: ParsedValues): Promise<{ options: RegistryOptions; explicit: Credentials | undefined }> {
  let explicit: Credentials | undefined;
  const username = str(v.username);
  if (v['password-stdin']) {
    if (!username) usage('--password-stdin needs --username');
    explicit = { username, password: await readStdin() };
  } else if (username) {
    usage('--username needs --password-stdin (passwords are not accepted on the command line)');
  }
  const timeoutMs = positive(str(v.timeout), '--timeout');
  const maxBlob = positive(str(v['max-download-bytes']), '--max-download-bytes');
  const network = networkOption(v);
  return {
    explicit,
    options: {
      insecureRegistries: insecureRegistriesFromEnv(strs(v['insecure-registry'])),
      allowPrivateNetwork: !!v['allow-private-network'],
      limits: { ...(timeoutMs ? { timeoutMs } : {}), ...(maxBlob ? { maxBlobBytes: maxBlob } : {}) },
      ...(network ? { network } : {}),
    },
  };
}

const push: Command = {
  name: 'push',
  summary: 'push an image to a registry',
  usage: 'push <registry/name[:tag]> [options]',
  description:
    'Push the local image of that name to an OCI registry (distribution spec v1.1): blobs the registry lacks\nare uploaded (layers of a BASE image are mounted from its repository when it is on the same registry),\nthen the manifest. Name the image first: sitefile tag blog:1 localhost:5000/blog:1.\nThe registry host is always explicit (there is no Docker Hub default for push: use docker.io/user/name).\nPlain http is allowed for localhost and --insecure-registry hosts only.',
  flags: COMMON_FLAGS,
  options: COMMON_OPTIONS,
  async run(v, pos) {
    requireArgs(pos, 1, 1, 'an image reference');
    const input = pos[0] as string;
    const ref0 = parseRef(input);
    if (ref0.registry === undefined) {
      usage(`'${input}' has no registry host; tag the image first, e.g. sitefile tag ${input} localhost:5000/${input}`);
    }
    const ref = canonicalRef(ref0);
    const store = await Store.open();
    const refs = await store.refs();
    const id = ref.digest !== undefined && ref.tag === undefined ? ref.digest : refs[refKey({ ...ref, digest: undefined })];
    if (id === undefined || !(await store.hasImage(id))) {
      throw new SitefileError('E_NOT_FOUND', `no local image ${refKey({ ...ref, digest: undefined })}; tag one first: sitefile tag <image> ${refKey({ ...ref, digest: undefined })}`);
    }
    const { options, explicit } = await clientOptions(v);
    const client = makeClient(ref.registry as string, { ...options, explicitCredentials: explicit });
    process.stdout.write(`The push refers to repository [${ref.registry}/${ref.name}]\n`);
    const r = await pushImage(store, client, id, ref, { onEvent: (m) => process.stdout.write(`${m}\n`) });
    process.stdout.write(`${ref.tag ?? 'digest'}: digest: ${r.digest} size: ${r.size}\n`);
    return 0;
  },
};

const pull: Command = {
  name: 'pull',
  summary: 'pull an image from a registry',
  usage: 'pull <[registry/]name[:tag][@sha256:...]> [options]',
  description:
    'Pull an image into the local store and tag it name:tag as written. A name without a registry host\nmeans Docker Hub (nginx -> docker.io/library/nginx). For a multi-platform image the --platform variant is\nchosen (default linux/amd64). Every blob is sha256-verified while it streams in, and a manifest fetched by\ndigest must hash to it. The result is stored as-is (Docker or OCI manifests); only images built by\nsitefile can be served or saved. Pin with @sha256:... for reproducibility.',
  flags: ['--platform <os/arch>   variant to choose from an image index (default linux/amd64)', '--max-download-bytes <n>   per-blob size limit (default 2 GiB)', ...COMMON_FLAGS],
  options: { ...COMMON_OPTIONS, platform: { type: 'string' }, 'max-download-bytes': { type: 'string' } },
  async run(v, pos) {
    requireArgs(pos, 1, 1, 'an image reference');
    const ref = withDefaultRegistry(parseRef(pos[0] as string));
    const store = await Store.open();
    const { options, explicit } = await clientOptions(v);
    const client = makeClient(ref.registry as string, { ...options, explicitCredentials: explicit });
    const platform = str(v.platform);
    const r = await pullImage(store, client, ref, {
      platform,
      enforcePlatform: platform !== undefined,
      tag: ref.digest === undefined,
      onEvent: (m) => process.stdout.write(`${m}\n`),
    });
    process.stdout.write(`Digest: ${r.rootDigest}\n`);
    process.stdout.write(`Pulled ${ref.digest === undefined ? refKey(ref) : `${ref.registry}/${ref.name}@${ref.digest}`} (image ${shortId(r.id)})\n`);
    return 0;
  },
};

export const registryCommands: ReadonlyMap<string, Command> = new Map([
  ['push', push],
  ['pull', pull],
]);

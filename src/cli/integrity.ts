import { promises as fs } from 'node:fs';
import { SitefileError } from '../errors.js';
import { diffText, diffTrees, loadTarget, manifestOf, parseManifest, pathInside, resolveTarget, verifyText, verifyTree, type Manifest } from '../integrity.js';
import { Store } from '../store.js';
import { requireArgs, str, usage, type Command } from './common.js';

const openStore = (): (() => Promise<Store>) => {
  let s: Promise<Store> | undefined;
  return () => (s ??= Store.open());
};

const verify: Command = {
  name: 'verify',
  summary: 'check a directory, archive or image against a file manifest',
  usage: 'verify <dir|archive|ref|id> [--manifest <file>] [--json]',
  description:
    'Compare a deployed or extracted copy with the manifest written by "build --manifest" or "save --manifest":\nevery file the manifest lists must be there with the same size and SHA-256, and no other file may be.\nPrints one line per difference (missing, changed, extra) and fails with E_INTEGRITY when there is any.\nA directory is read like a build context (hidden files count; .git is skipped); an archive is read from\nits bytes (zip, tar, tar.gz, wbn, OCI archive). A manifest file inside the verified directory is not\ncounted as extra. Without --manifest the target must be an image: its stored blobs are checked.',
  flags: ['--manifest <file>  the manifest to check against (JSON, see docs/CLI.md)', '--json             machine-readable report'],
  options: { manifest: { type: 'string' }, json: { type: 'boolean' } },
  async run(v, pos) {
    requireArgs(pos, 1, 1, 'a directory, archive file or image reference');
    const arg = pos[0] as string;
    const store = openStore();
    const target = await resolveTarget(arg, store);
    const manifestFile = str(v.manifest);
    if (manifestFile === '') usage('--manifest expects a file path');
    if (manifestFile === undefined && target.kind !== 'image') usage('verify needs --manifest <file> for a directory or an archive');
    const tree = await loadTarget(target, store);
    let manifest: Manifest;
    let ignore: string | undefined;
    if (manifestFile !== undefined) {
      const bytes = await fs.readFile(manifestFile).catch((e: NodeJS.ErrnoException) => {
        throw new SitefileError(e.code === 'ENOENT' ? 'E_NOT_FOUND' : 'E_INTEGRITY', `cannot read the manifest '${manifestFile}'`);
      });
      manifest = parseManifest(new Uint8Array(bytes), manifestFile);
      if (target.kind === 'dir') ignore = await pathInside(target.path, manifestFile);
    } else {
      manifest = manifestOf(tree); // loadTree checked every blob against its digest
    }
    const r = verifyTree(tree, manifest, ignore);
    process.stdout.write(v.json ? JSON.stringify(r, null, 2) + '\n' : r.ok ? `OK: ${r.files} file${r.files === 1 ? '' : 's'} match ${r.root}\n` : verifyText(r));
    if (!r.ok) {
      const message = `'${arg}' does not match the manifest: ${r.missing.length} missing, ${r.changed.length} changed, ${r.extra.length} extra`;
      // With --json the report above is the one document on stdout (it already says ok: false),
      // so the error goes to stderr only instead of through the generic --json error output.
      if (v.json) {
        process.stderr.write(`error[E_INTEGRITY]: ${message}\n`);
        return 1;
      }
      throw new SitefileError('E_INTEGRITY', message);
    }
    return 0;
  },
};

const diff: Command = {
  name: 'diff',
  summary: 'files changed between two images, directories or archives',
  usage: 'diff <a> <b> [--json] [--exit-code]',
  description:
    'List the files added (A), removed (D) and changed (C: content or mode) going from <a> to <b>, with their\nsizes, then the rules added (+) and removed (-) in _redirects and _headers. Each side is a directory, an\narchive file (zip, tar, tar.gz, wbn, OCI archive) or an image reference or ID, read as "sitefile verify"\nreads it (hidden files count). Digests, not timestamps, decide what changed.',
  flags: ['--json        machine-readable report (sizes in bytes, digests, modes)', '--exit-code   exit 1 when the two differ (default 0 either way)'],
  options: { json: { type: 'boolean' }, 'exit-code': { type: 'boolean' } },
  async run(v, pos) {
    requireArgs(pos, 2, 2, 'two images, directories or archive files');
    const store = openStore();
    const [a, b] = [await resolveTarget(pos[0] as string, store), await resolveTarget(pos[1] as string, store)];
    const r = diffTrees(await loadTarget(a, store), await loadTarget(b, store));
    process.stdout.write(v.json ? JSON.stringify(r, null, 2) + '\n' : diffText(r));
    const same = !r.added.length && !r.removed.length && !r.changed.length && !Object.keys(r.rules).length;
    return v['exit-code'] && !same ? 1 : 0;
  },
};

export const integrityCommands: ReadonlyMap<string, Command> = new Map([
  ['verify', verify],
  ['diff', diff],
]);

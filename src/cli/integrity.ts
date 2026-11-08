import { promises as fs } from 'node:fs';
import { SitefileError } from '../errors.js';
import { loadTarget, manifestOf, parseManifest, pathInside, resolveTarget, verifyText, verifyTree, type Manifest } from '../integrity.js';
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
      throw new SitefileError('E_INTEGRITY', `'${arg}' does not match the manifest: ${r.missing.length} missing, ${r.changed.length} changed, ${r.extra.length} extra`);
    }
    return 0;
  },
};

export const integrityCommands: ReadonlyMap<string, Command> = new Map([['verify', verify]]);

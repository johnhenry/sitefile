import { promises as fs } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { sourceDateEpoch, writeTar } from '../archive-writers.js';
import { SitefileError } from '../errors.js';
import { exportDirectory, exportFile } from '../exporter.js';
import { compressionFromExtension, encodeTree, formatFromExtension, isArchiveFormat } from '../export-formats.js';
import { EXPORT_FORMATS, parseExportFormat, type ExportFormat } from '../language/ast.js';
import { MEDIA, siteLayerIndex } from '../oci.js';
import { looksLikeImageId, parseLocalRef, refKey } from '../ref.js';
import { shortId, Store, type ImageSummary } from '../store.js';
import { encodeManifest, manifestOf } from '../integrity.js';
import { humanSize, relativeTime, requireArgs, str, table, usage, type Command } from './common.js';

function split(key: string): [string, string] {
  const i = key.lastIndexOf(':');
  return [key.slice(0, i), key.slice(i + 1)];
}

/** A path relative to the cwd when it is inside it, else absolute. */
function shown(p: string): string {
  const rel = path.relative(process.cwd(), p);
  return rel === '' ? '.' : rel.startsWith('..') ? p : rel;
}

const images: Command = {
  name: 'images',
  summary: 'list images in the local store',
  usage: 'images [--json] [--digests] [-q]',
  description: 'List the images in the local store (SITEFILE_HOME or ~/.sitefile), newest first.\nImages without a tag show as <none>. Alias: sitefile image ls.',
  flags: ['--json         machine-readable output (full IDs, sizes in bytes)', '--digests      add the full image ID (sha256:...) column', '-q, --quiet    only print short image IDs'],
  options: { json: { type: 'boolean' }, digests: { type: 'boolean' }, quiet: { type: 'boolean', short: 'q' } },
  async run(v, pos) {
    requireArgs(pos, 0, 0, 'no arguments');
    const store = await Store.open();
    const list = await store.list();
    const rows = list.flatMap((im) => (im.tags.length ? im.tags : [undefined]).map((t) => ({ im, tag: t })));
    if (v.json) {
      const out = rows.map(({ im, tag }) => {
        const [repository, t] = tag ? split(tag) : [null, null];
        return { repository, tag: t, id: im.id, created: im.created, files: im.files, size: im.size };
      });
      process.stdout.write(JSON.stringify(out, null, 2) + '\n');
      return 0;
    }
    if (v.quiet) {
      const seen = new Set<string>();
      for (const { im } of rows) if (!seen.has(im.id)) (seen.add(im.id), process.stdout.write(`${shortId(im.id)}\n`));
      return 0;
    }
    const head = ['REPOSITORY', 'TAG', 'IMAGE ID', ...(v.digests ? ['DIGEST'] : []), 'CREATED', 'FILES', 'SIZE'];
    const body = rows.map(({ im, tag }) => {
      const [repo, t] = tag ? split(tag) : ['<none>', '<none>'];
      return [repo, t, shortId(im.id), ...(v.digests ? [im.id] : []), relativeTime(im.created), String(im.files), humanSize(im.size)];
    });
    process.stdout.write(table([head, ...body]));
    return 0;
  },
};

const inspect: Command = {
  name: 'image inspect',
  summary: 'show an image manifest',
  usage: 'image inspect <ref|id> [--json]',
  description: 'Show an image: its ID, tags, creation time, build metadata (ARG names only) and files.',
  flags: ['--json   machine-readable output'],
  options: { json: { type: 'boolean' } },
  async run(v, pos) {
    requireArgs(pos, 1, 1, 'an image reference');
    const store = await Store.open();
    const id = await store.resolve(pos[0] as string);
    const d = await store.details(id);
    const tree = await store.loadTree(id); // verifies every digest
    const files = tree.entries().map(([p, n]) => ({ path: p, mode: n.mode.toString(8), size: n.bytes.byteLength, digest: n.digest }));
    const layer = d.manifest.layers[0];
    if (v.json) {
      const out = {
        id,
        tags: d.tags,
        created: d.created,
        os: d.config.os,
        architecture: d.config.architecture,
        root: d.root,
        size: d.size,
        labels: d.config.labels,
        config: d.manifest.config,
        layers: d.manifest.layers,
        diffIds: d.config.diffIds,
        files,
      };
      process.stdout.write(JSON.stringify(out, null, 2) + '\n');
      return 0;
    }
    const labels = Object.entries(d.config.labels).map(([k, val]) => `  ${k}=${val}`);
    const out = [
      `ID:          ${id}`,
      `Tags:        ${d.tags.join(', ') || '<none>'}`,
      `Stored:      ${d.created} (${relativeTime(d.created)})`,
      `Platform:    ${d.config.os}/${d.config.architecture}`,
      `Root:        ${d.root}`,
      `Layer:       ${layer?.digest ?? '-'} (${humanSize(d.layerSize)} ${d.manifest.layers[siteLayerIndex(d.manifest, d.config)]?.mediaType === MEDIA.layerTar ? 'tar' : 'gzip'})`,
      `Diff ID:     ${d.config.diffIds[0] ?? '-'}`,
      `Config:      ${d.manifest.config.digest}`,
      'Labels:',
      ...labels,
      `Files:       ${files.length} (${humanSize(d.size)})`,
      '',
      ...files.map((f) => `${f.mode}  ${String(f.size).padStart(8)}  ${f.path}`),
    ];
    process.stdout.write(out.join('\n') + '\n');
    return 0;
  },
};

const tag: Command = {
  name: 'tag',
  summary: 'add a name:tag to an image',
  usage: 'tag <src> <dst>',
  description: 'Create the local reference <dst> (name[:tag]) for the image <src> (reference or ID).\nAn existing <dst> is moved to the new image.',
  flags: [],
  options: {},
  async run(_v, pos) {
    requireArgs(pos, 2, 2, '<src> and <dst>');
    const dst = parseLocalRef(pos[1] as string);
    const store = await Store.open();
    const id = await store.resolve(pos[0] as string);
    const key = await store.tag(id, dst);
    process.stdout.write(`${key} -> ${shortId(id)}\n`);
    return 0;
  },
};

const rmi: Command = {
  name: 'rmi',
  summary: 'remove tags / images',
  usage: 'rmi [-f] <ref|id>...',
  description:
    'Remove the tag <ref>. The image manifest is deleted when no other tag references it (blobs are\nreclaimed by "sitefile image prune"). Removing by image ID untags every tag and needs -f when there is more than one.',
  flags: ['-f, --force   remove an image by ID even if it has several tags'],
  options: { force: { type: 'boolean', short: 'f' } },
  async run(v, pos) {
    requireArgs(pos, 1, Infinity, 'an image reference');
    const store = await Store.open();
    let failed = false;
    for (const ref of pos) {
      try {
        const r = await store.removeImage(ref, !!v.force);
        for (const t of r.untagged) process.stdout.write(`Untagged: ${t}\n`);
        if (r.deleted) process.stdout.write(`Deleted: ${r.deleted}\n`);
      } catch (e) {
        if (!(e instanceof SitefileError)) throw e;
        if (e.code === 'E_USAGE') throw e;
        process.stderr.write(`error[${e.code}]: ${e.message}\n`);
        failed = true;
      }
    }
    return failed ? 1 : 0;
  },
};

const prune: Command = {
  name: 'image prune',
  summary: 'delete unreferenced manifests and blobs',
  usage: 'image prune',
  description: 'Delete untagged image manifests and every blob no remaining image uses, then print the space reclaimed.',
  flags: [],
  options: {},
  async run(_v, pos) {
    requireArgs(pos, 0, 0, 'no arguments');
    const store = await Store.open();
    const r = await store.prune();
    process.stdout.write(`Deleted ${r.manifests} image${r.manifests === 1 ? '' : 's'} and ${r.blobs} blob${r.blobs === 1 ? '' : 's'}\nTotal reclaimed space: ${humanSize(r.bytes)}\n`);
    return 0;
  },
};

const save: Command = {
  name: 'save',
  summary: 'export an image to a directory or archive',
  usage: 'save <ref|id> -o <out> [--format fmt] [--base-url url] [--manifest file] [--force]',
  description:
    'Write an image\'s files to a directory, or to a zip, tar, tar.gz (tgz) or wbn archive, or the image itself as\nan OCI archive (-o x.oci.tar) or OCI layout directory (--format oci). The format is chosen like "build -o":\nfrom --format, else the extension (.zip .tar .tar.gz .tgz .wbn .wbn.gz .oci.tar), else a directory; a wbn\nwritten to *.wbn.gz is gzipped.\nArchives are deterministic (set SOURCE_DATE_EPOCH to stamp them). A non-empty destination needs --force.',
  flags: [
    '-o, --out <path>     destination (required)',
    `    --format <fmt>   ${EXPORT_FORMATS.join(' | ')} (alias tar.gz)`,
    '    --base-url <url> https URL ending in "/" (wbn only; required for wbn)',
    '    --manifest <file> also write a manifest of the files (see sitefile verify)',
    '    --force          replace a non-empty destination or an existing file',
  ],
  options: { out: { type: 'string', short: 'o' }, format: { type: 'string' }, 'base-url': { type: 'string' }, manifest: { type: 'string' }, force: { type: 'boolean' } },
  async run(v, pos) {
    requireArgs(pos, 1, 1, 'an image reference');
    const out = str(v.out);
    if (!out) usage('save requires -o <out>');
    let format: ExportFormat | undefined;
    if (v.format !== undefined) {
      format = parseExportFormat(str(v.format) ?? '');
      if (!format) usage(`--format expects one of ${EXPORT_FORMATS.join(', ')} (or tar.gz)`);
    }
    format ??= formatFromExtension(out) ?? 'directory';
    const manifest = str(v.manifest);
    if (manifest === '') usage('--manifest expects a file path');
    if (manifest !== undefined && format === 'directory') {
      const rel = path.relative(path.resolve(out), path.resolve(manifest));
      if (rel !== '' && !rel.startsWith('..') && !path.isAbsolute(rel)) usage(`--manifest '${manifest}' is inside the exported directory '${out}': write it elsewhere`);
    }
    const store = await Store.open();
    const id = await store.resolve(pos[0] as string);
    // An empty scratch directory as the "context": the destination is then always
    // outside it, so a non-empty directory or an existing file needs --force.
    const scratch = await fs.mkdtemp(path.join(os.tmpdir(), 'sitefile-save-'));
    try {
      const opts = { contextDir: scratch, ...(v.force ? { force: true } : {}) };
      const writeManifest = async (): Promise<void> => {
        if (manifest !== undefined) await exportFile(encodeManifest(manifestOf(await store.loadTree(id))), manifest, opts);
      };
      if (format === 'oci') {
        // The stored image as-is (same digests): an OCI archive for *.tar, else a layout directory.
        const input = pos[0] as string;
        const refName = looksLikeImageId(input) ? undefined : refKey(parseLocalRef(input));
        const layout = await store.layoutOf(id, refName);
        if (out.toLowerCase().endsWith('.tar')) {
          const w = await exportFile(writeTar(layout, { epoch: sourceDateEpoch() ?? 0 }), out, opts);
          process.stdout.write(`Saved ${shortId(id)} to ${shown(w.outFile)} (oci archive, ${w.bytes} bytes)\n`);
        } else {
          const w = await exportDirectory(layout, out, opts);
          process.stdout.write(`Saved ${shortId(id)} to ${shown(w.outDir)} (oci layout)\n`);
        }
        await writeManifest();
        return 0;
      }
      const tree = await store.loadTree(id);
      if (!isArchiveFormat(format)) {
        const w = await exportDirectory(tree, out, opts);
        process.stdout.write(`Saved ${shortId(id)} (${tree.size} files) to ${shown(w.outDir)}\n`);
      } else {
        const data = encodeTree(tree, format, { epoch: sourceDateEpoch(), baseUrl: str(v['base-url']), compression: compressionFromExtension(out, format) });
        const w = await exportFile(data, out, opts);
        process.stdout.write(`Saved ${shortId(id)} (${tree.size} files) to ${shown(w.outFile)} (${format === 'tgz' ? 'tar.gz' : format}, ${w.bytes} bytes)\n`);
      }
      await writeManifest();
    } finally {
      await fs.rm(scratch, { recursive: true, force: true });
    }
    return 0;
  },
};

const diff: Command = {
  name: 'image diff',
  summary: 'compare two images',
  usage: 'image diff <ref|id> <ref|id>',
  description: 'List paths added (A), removed (D) or changed (C) going from the first image to the second.\nExit status is 0 whether or not they differ.',
  flags: [],
  options: {},
  async run(_v, pos) {
    requireArgs(pos, 2, 2, 'two image references');
    const store = await Store.open();
    const [a, b] = await Promise.all(pos.map(async (p) => store.loadTree(await store.resolve(p))));
    const am = new Map(a?.entries());
    const bm = new Map(b?.entries());
    const lines: string[] = [];
    for (const p of [...new Set([...am.keys(), ...bm.keys()])].sort()) {
      const x = am.get(p);
      const y = bm.get(p);
      if (!x) lines.push(`A ${p}`);
      else if (!y) lines.push(`D ${p}`);
      else if (x.digest !== y.digest || x.mode !== y.mode) lines.push(`C ${p}`);
    }
    process.stdout.write(lines.length ? lines.join('\n') + '\n' : '');
    return 0;
  },
};

export const imageCommands: ReadonlyMap<string, Command> = new Map([
  ['images', images],
  ['tag', tag],
  ['rmi', rmi],
  ['save', save],
  ['image inspect', inspect],
  ['image prune', prune],
  ['image diff', diff],
]);

/** `sitefile image <sub>` → command; `ls`, `rm`, `tag` and `save` map onto the top-level commands. */
export const imageSubcommands: ReadonlyMap<string, Command> = new Map([
  ['ls', images],
  ['list', images],
  ['rm', rmi],
  ['tag', tag],
  ['save', save],
  ['inspect', inspect],
  ['prune', prune],
  ['diff', diff],
]);

export type { ImageSummary };

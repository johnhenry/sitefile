import { writeTar, writeTgz, writeZip, type WriterOptions } from './archive-writers.js';
import type { ExportFormat } from './language/ast.js';
import type { Tree } from './tree.js';
import { writeWebBundle } from './wbn.js';

export type ArchiveFormat = Exclude<ExportFormat, 'directory'>;

export function isArchiveFormat(f: ExportFormat): f is ArchiveFormat {
  return f !== 'directory';
}

/** Default output path relative to the context: `dist`, `dist.zip`, `dist.tar`, `dist.tar.gz`, `dist.wbn`. */
export function defaultOutName(format: ExportFormat): string {
  switch (format) {
    case 'directory':
      return 'dist';
    case 'zip':
      return 'dist.zip';
    case 'tar':
      return 'dist.tar';
    case 'tgz':
      return 'dist.tar.gz';
    case 'wbn':
      return 'dist.wbn';
  }
}

export interface EncodeOptions extends WriterOptions {
  /** Required for `wbn`: absolute https URL ending in `/`. */
  readonly baseUrl?: string | undefined;
}

/** Serialize a tree in an archive format. Deterministic: see archive-writers.ts and wbn.ts. */
export function encodeTree(tree: Tree, format: ArchiveFormat, options: EncodeOptions = {}): Uint8Array {
  switch (format) {
    case 'zip':
      return writeZip(tree, options);
    case 'tar':
      return writeTar(tree, options);
    case 'tgz':
      return writeTgz(tree, options);
    case 'wbn':
      return writeWebBundle(tree, options.baseUrl);
  }
}

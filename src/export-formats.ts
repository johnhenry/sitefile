import { writeTar, writeTgz, writeZip, type WriterOptions } from './archive-writers.js';
import { gzip } from './deflate.js';
import type { ExportFormat, OciCompression } from './language/ast.js';
import type { Tree } from './tree.js';
import { writeWebBundle } from './wbn.js';
import { writeCar } from './car.js';
import { writeMhtml, writeSingleFile } from './single-file.js';
import { writeWacz } from './wacz.js';
import { writeWarc } from './warc.js';

export type ArchiveFormat = Exclude<ExportFormat, 'directory' | 'oci'>;

/** Single-file formats encoded by {@link encodeTree}. `oci` is handled separately (layout directory or OCI archive). */
export function isArchiveFormat(f: ExportFormat): f is ArchiveFormat {
  return f !== 'directory' && f !== 'oci';
}

/**
 * Default output path relative to the context: `dist`, `dist.zip`, `dist.tar`, `dist.tar.gz`, `dist.wbn`
 * (`dist.wbn.gz` for a gzipped bundle), `dist.oci.tar`, `dist.car`, `dist.html`, `dist.mhtml`, `dist.warc` (`dist.warc.gz` gzipped), `dist.wacz`.
 */
export function defaultOutName(format: ExportFormat, compression?: OciCompression): string {
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
      return compression === 'gzip' ? 'dist.wbn.gz' : 'dist.wbn';
    case 'oci':
      return 'dist.oci.tar';
    case 'car':
      return 'dist.car';
    case 'html':
      return 'dist.html';
    case 'mhtml':
      return 'dist.mhtml';
    case 'warc':
      return compression === 'gzip' ? 'dist.warc.gz' : 'dist.warc';
    case 'wacz':
      return 'dist.wacz';
  }
}

export interface EncodeOptions extends WriterOptions {
  /** Required for `wbn`: absolute https URL ending in `/`; for `mhtml`, the parts' URLs (default https://site.invalid/). */
  readonly baseUrl?: string | undefined;
  /** `wbn`: `gzip` wraps the finished bundle in gzip (the shared deterministic encoder); default none. */
  readonly compression?: OciCompression | undefined;
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
    case 'wbn': {
      // The bundle is written first, unchanged; gzip only wraps its bytes (header mtime 0, see deflate.ts).
      const bundle = writeWebBundle(tree, options.baseUrl);
      return options.compression === 'gzip' ? gzip(bundle) : bundle;
    }
    case 'car':
      return writeCar(tree);
    case 'html':
      return writeSingleFile(tree);
    case 'mhtml':
      return writeMhtml(tree, undefined, options.baseUrl);
    case 'warc':
      return writeWarc(tree, { baseUrl: options.baseUrl, epoch: options.epoch, compression: options.compression });
    case 'wacz':
      return writeWacz(tree, { baseUrl: options.baseUrl, epoch: options.epoch });
  }
}

/** The export format implied by an output file name, if it has an archive extension. */
export function formatFromExtension(out: string): ExportFormat | undefined {
  const name = out.toLowerCase();
  if (name.endsWith('.oci.tar')) return 'oci';
  if (name.endsWith('.tar.gz') || name.endsWith('.tgz')) return 'tgz';
  if (name.endsWith('.tar')) return 'tar';
  if (name.endsWith('.zip')) return 'zip';
  if (name.endsWith('.wbn') || name.endsWith('.wbn.gz')) return 'wbn';
  if (name.endsWith('.car')) return 'car';
  if (name.endsWith('.warc') || name.endsWith('.warc.gz')) return 'warc';
  if (name.endsWith('.wacz')) return 'wacz';
  if (name.endsWith('.mhtml') || name.endsWith('.mht')) return 'mhtml';
  return undefined;
}

/** The compression implied by an output file name for `format`: `gzip` for a wbn written to `*.wbn.gz`. */
export function compressionFromExtension(out: string, format: ExportFormat): OciCompression | undefined {
  const name = out.toLowerCase();
  if (format === 'wbn') return name.endsWith('.wbn.gz') ? 'gzip' : undefined;
  return format === 'warc' && name.endsWith('.warc.gz') ? 'gzip' : undefined;
}

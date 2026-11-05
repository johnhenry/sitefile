import { sourceDateEpoch } from '../archive-writers.js';
import { SitefileError } from '../errors.js';
import type { BuildResult } from '../executor.js';
import { imageMeta } from '../oci.js';
import { parseLocalRef } from '../ref.js';
import { shortId, Store } from '../store.js';

/** Validate `-t` values before building: local name[:tag] only. Throws E_USAGE / E_UNSUPPORTED. */
export function validateImageTags(tags: readonly string[]): void {
  for (const t of tags) {
    const ref = parseLocalRef(t);
    if (ref.digest !== undefined) throw new SitefileError('E_USAGE', `-t '${t}': a tag cannot include a digest`);
  }
}

/**
 * Store a build's exported tree as an OCI image in the local store and point each tag at it.
 * Build-arg values are never recorded (nothing but the files and OCI labels reach the image).
 */
export async function storeBuildAsImage(
  result: BuildResult,
  tags: readonly string[],
  info: {
    readonly contextDir: string;
    readonly ociRoot?: string | undefined;
    readonly platform?: string | undefined;
    readonly labels?: Readonly<Record<string, string>> | undefined;
    readonly gitLabels?: boolean | undefined;
  },
): Promise<string> {
  const store = await Store.open();
  const { id } = await store.addImage(result.output, {
    root: info.ociRoot ?? result.plan.exportRoot,
    platform: info.platform,
    epoch: sourceDateEpoch(),
    base: result.base,
    ...(await imageMeta(info.contextDir, { git: !!info.gitLabels, ...(info.labels ? { labels: info.labels } : {}) })),
  });
  for (const t of tags) {
    const key = await store.tag(id, parseLocalRef(t));
    process.stdout.write(`Tagged ${key} ${shortId(id)} (${result.output.size} file${result.output.size === 1 ? '' : 's'})\n`);
  }
  return id;
}

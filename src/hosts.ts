import { isSitefileError, needsHost, type Needs, type SourceSpan } from './errors.js';
import { loadProgram, type BuildOptions } from './executor.js';
import { plan } from './planner.js';
import { parseRef, registryApiHost, withDefaultRegistry } from './ref.js';
import { checkSecretHosts, hostAllowed, hostMatches, urlPort, type NetworkSettings } from './resources/network.js';

/**
 * `sitefile hosts` (issue #176): the hosts a build would contact, found from the plan alone (nothing is
 * fetched), with what each is needed for and whether the current host rules allow it.
 *
 * What is listed: every remote IMPORT (https and git+https: the URL's host), every `FROM image` stage base
 * with a registry, and the BASE image of an oci export (or `--base`): the registry's API host. What is not
 * known before fetching, and so not listed: redirects, a registry's token realm and blob CDN, the inputs of
 * an imported Sitefile (`IMPORT … AS sitefile`, marked `nested`), and the links `CHECKLINKS EXTERNAL`
 * checks (reported as `checklinksExternal`). Those still come up one at a time at build time, each with an
 * E_HOST_BLOCKED `needs`.
 */
export type HostStatus = 'allowed' | 'blocked' | 'denied';

export interface HostUse {
  readonly kind: 'import' | 'image' | 'base';
  /** The IMPORT name, the stage of a `FROM image` (its name, or `#<n>`), or `BASE`. */
  readonly name: string;
  /** The URL, or the canonical image reference. */
  readonly source: string;
  /** The instruction (null for a BASE given with --base or on EXPORT). */
  readonly span: SourceSpan | null;
  /** `IMPORT … SECRET` ids sent to this host. */
  readonly secrets: readonly string[];
  /** An imported Sitefile: its own inputs are only known once it is fetched. */
  readonly nested: boolean;
  /** Strict mode: the `--allow-secret-host` rule this use still needs (the first missing one). */
  readonly needs: Needs | null;
}

export interface HostEntry {
  /** As written in the URL (lowercase, IPv6 without brackets). */
  readonly host: string;
  readonly port: number;
  readonly status: HostStatus;
  /** For `blocked`: the `--allow-hosts` that allows it. */
  readonly needs: Needs | null;
  readonly uses: readonly HostUse[];
}

export interface HostsReport {
  readonly strict: boolean;
  readonly hosts: readonly HostEntry[];
  /** Lines of `CHECKLINKS --external`, which contacts the hosts of the site's external links. */
  readonly checklinksExternal: readonly number[];
}

function statusOf(host: string, port: number, network: NetworkSettings | undefined): HostStatus {
  if (hostAllowed(host, port, network)) return 'allowed';
  return (network?.denyHosts ?? []).some((p) => hostMatches(host, port, p)) ? 'denied' : 'blocked';
}

function secretNeeds(name: string, ids: readonly string[], target: string, network: NetworkSettings | undefined, span: SourceSpan): Needs | null {
  try {
    checkSecretHosts(name, ids, target, network, span);
    return null;
  } catch (e) {
    if (isSitefileError(e) && e.needs) return e.needs;
    throw e;
  }
}

export async function hosts(options: BuildOptions): Promise<HostsReport> {
  const { program } = await loadProgram(options);
  const p = plan(program, options.buildArgs ?? {});
  p.warnings.forEach((m) => options.onWarning?.(m));
  const network = options.fetchPolicy?.network;
  const entries: Array<{ host: string; port: number; uses: HostUse[] }> = [];
  const add = (host: string, port: number, use: HostUse) => {
    const h = host.toLowerCase().replace(/^\[|\]$/g, '').replace(/\.$/, '');
    let e = entries.find((x) => x.host === h && x.port === port);
    if (!e) entries.push((e = { host: h, port, uses: [] }));
    e.uses.push(use);
  };
  for (const spec of p.imports) {
    const loc = spec.location;
    if (loc.kind === 'local') continue;
    const target = loc.kind === 'remote' ? loc.url : loc.repo;
    let url: URL;
    try {
      url = new URL(target);
    } catch {
      continue;
    }
    const ids = [...new Set((spec.secrets ?? []).map((s) => s.id))];
    add(url.hostname, urlPort(url), {
      kind: 'import',
      name: spec.name,
      source: loc.url,
      span: spec.span,
      secrets: ids,
      nested: spec.as === 'sitefile',
      needs: secretNeeds(spec.name, ids, target, network, spec.span),
    });
  }
  const image = (kind: 'image' | 'base', name: string, input: string, span: SourceSpan | null) => {
    const ref = withDefaultRegistry(parseRef(input));
    const registry = ref.registry as string;
    const source = `${registry}/${ref.name}${ref.tag !== undefined ? `:${ref.tag}` : ''}${ref.digest !== undefined ? `@${ref.digest}` : ''}`;
    const api = registryApiHost(registry);
    const m = /^(.*?)(?::(\d+))?$/.exec(api) as RegExpExecArray;
    add(m[1] as string, m[2] !== undefined ? Number(m[2]) : 443, { kind, name, source, span, secrets: [], nested: false, needs: null });
  };
  for (const [i, st] of p.stages.entries()) {
    if (st.base.kind === 'image' && !st.base.local) image('image', st.name ?? `#${i + 1}`, st.base.ref, st.base.span);
  }
  const baseInput = options.baseImage ?? (p.exportFormat === 'oci' ? p.exportBase : undefined);
  if (baseInput !== undefined) image('base', 'BASE', baseInput, null);
  const checklinksExternal = p.stages.flatMap((st) => st.operations.flatMap((op) => (op.kind === 'CHECKLINKS' && op.external ? [op.span.line] : [])));
  return {
    strict: !!network?.strict,
    hosts: entries.map((e) => {
      const status = statusOf(e.host, e.port, network);
      return { host: e.host, port: e.port, status, needs: status === 'blocked' ? needsHost(e.host) : null, uses: e.uses };
    }),
    checklinksExternal,
  };
}

/** `HostsReport` as JSON, keys in the documented order (compared across implementations). */
export function hostsJson(r: HostsReport): string {
  const needs = (n: Needs | null) => (n ? { kind: n.kind, name: n.name, flag: n.flag } : null);
  return (
    JSON.stringify(
      {
        strict: r.strict,
        hosts: r.hosts.map((h) => ({
          host: h.host,
          port: h.port,
          status: h.status,
          needs: needs(h.needs),
          uses: h.uses.map((u) => ({
            kind: u.kind,
            name: u.name,
            source: u.source,
            span: u.span ? { file: u.span.file, line: u.span.line, column: u.span.column } : null,
            secrets: u.secrets,
            nested: u.nested,
            needs: needs(u.needs),
          })),
        })),
        checklinksExternal: r.checklinksExternal,
      },
      null,
      2,
    ) + '\n'
  );
}

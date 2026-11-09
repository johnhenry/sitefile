import { NETWORK_OPTIONS, networkHelp, networkOption } from './network.js';
import { collectBuildArgs } from '../build-args.js';
import { hosts, hostsJson, type HostsReport, type HostUse } from '../hosts.js';
import { requireArgs, str, strs, table, type Command } from './common.js';

function usedBy(u: HostUse): string {
  const what = u.kind === 'import' ? `IMPORT ${u.name}` : u.kind === 'image' ? `FROM image ${u.name}` : 'BASE';
  return u.span ? `${what} (${u.span.file}:${u.span.line})` : what;
}

/** The text report: one row per use, then what each refused host or secret needs, and a summary line. */
export function formatHosts(r: HostsReport): string {
  let out = '';
  if (r.hosts.length === 0) out = 'No remote inputs: the Sitefile names no host to contact\n';
  else {
    const rows = [['HOST', 'PORT', 'STATUS', 'USED BY']];
    for (const h of r.hosts) for (const u of h.uses) rows.push([h.host, String(h.port), h.status, usedBy(u)]);
    out = table(rows);
    for (const h of r.hosts) if (h.needs) out += `${h.host}: needs ${h.needs.flag} ${h.needs.name}\n`;
    for (const h of r.hosts) for (const u of h.uses) if (u.needs) out += `${usedBy(u)}: secret needs ${u.needs.flag} ${u.needs.name}\n`;
    for (const h of r.hosts) for (const u of h.uses) if (u.nested) out += `${usedBy(u)}: an imported Sitefile; the hosts it needs are known once it is fetched\n`;
    const n = r.hosts.length;
    const counts = (['allowed', 'blocked', 'denied'] as const).map((s) => [s, r.hosts.filter((h) => h.status === s).length] as const).filter(([, k]) => k > 0);
    out += `${n} host${n === 1 ? '' : 's'}${r.strict ? ' (strict network mode)' : ''}: ${counts.map(([s, k]) => `${k} ${s}`).join(', ')}\n`;
  }
  for (const line of r.checklinksExternal) out += `CHECKLINKS --external (line ${line}) also contacts the hosts of external links\n`;
  return out;
}

const hostsCommand: Command = {
  name: 'hosts',
  summary: 'list the hosts a build would contact, before contacting any',
  usage: 'hosts [context] [options]',
  description:
    "List the hosts a build of the Sitefile would contact (remote IMPORTs, FROM image registries, the BASE\nimage's registry), what needs each, and whether the network rules (--allow-hosts, --deny-hosts,\n--network strict, --allow-secret-host) allow it. Nothing is fetched. Redirects, registry token and blob\nhosts and the inputs of an imported Sitefile are only known while building.",
  flags: [
    '-f, --file <path>             Sitefile path (default: <context>/Sitefile)',
    '--json                        machine-readable output',
    '--build-arg <NAME=value>      set an ARG (repeatable); --build-arg NAME reads $NAME',
    '--build-arg-file <path>       read ARGs from a .json, .yaml/.yml or .env file (repeatable)',
    '--base <image>                include this BASE image (overrides EXPORT ... AS oci BASE)',
    ...networkHelp(30),
  ],
  options: {
    ...NETWORK_OPTIONS,
    file: { type: 'string', short: 'f' },
    json: { type: 'boolean' },
    'build-arg': { type: 'string', multiple: true },
    'build-arg-file': { type: 'string', multiple: true },
    base: { type: 'string' },
  },
  async run(v, pos) {
    requireArgs(pos, 0, 1, 'a context directory');
    const network = networkOption(v);
    const file = str(v.file);
    const base = str(v.base);
    const r = await hosts({
      contextDir: pos[0] ?? '.',
      ...(file ? { sitefile: file } : {}),
      buildArgs: await collectBuildArgs(strs(v['build-arg-file']), strs(v['build-arg'])),
      fetchPolicy: network ? { network } : {},
      ...(base !== undefined ? { baseImage: base } : {}),
      onWarning: (m) => process.stderr.write(`warning: ${m}\n`),
    });
    process.stdout.write(v.json ? hostsJson(r) : formatHosts(r));
    return 0;
  },
};

export const hostsCommands: ReadonlyMap<string, Command> = new Map([['hosts', hostsCommand]]);

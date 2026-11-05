import { runCommand, usageError, type Command } from './common.js';
import { imageCommands, imageSubcommands } from './images.js';
import { registryCommands } from './registry.js';
import { serverCommands, serverSubcommands } from './servers.js';
import { cacheSubcommands } from './cache.js';

/**
 * Image-store and site-server commands. They live outside cli.ts' build/validate/inspect
 * parser: each has its own options and `--help`.
 */
const TOP: ReadonlyMap<string, Command> = new Map([...imageCommands, ...serverCommands, ...registryCommands].filter(([k]) => !k.includes(' ')));

export const EXTRA_COMMANDS: readonly string[] = [...TOP.keys(), 'image', 'server', 'cache'];

export const EXTRA_HELP = `
Images (local store: $SITEFILE_HOME or ~/.sitefile):
  sitefile build [context] -t name[:tag]           also store the result as an image (repeatable -t)
  sitefile images [--json] [--digests] [-q]        list images (alias: image ls)
  sitefile image inspect <ref> [--json]            show an image manifest
  sitefile image prune                             delete unreferenced manifests and blobs
  sitefile image diff <a> <b>                      added/removed/changed paths
  sitefile tag <src> <dst>                         add a name:tag
  sitefile rmi [-f] <ref|id>...                    remove tags / images
  sitefile save <ref> -o out[.zip|.tar|.tar.gz|.wbn]   export an image
  sitefile push <registry/name[:tag]>              push a (tagged) image to an OCI registry
  sitefile pull <[registry/]name[:tag][@digest]>   pull an image (a bare name means Docker Hub)

Build cache (TRANSFORM results: $SITEFILE_HOME/cache/transform):
  sitefile cache info [--json]                     location, entries, size
  sitefile cache prune [--older-than <days>]       delete cached results

Serving sites (detached servers are tracked by name):
  sitefile serve [--name n] [-p [host:]port] [-d] [--spa] <ref|./dir>
  sitefile servers [-a] [--json]                   list servers (alias: server ls)
  sitefile server stop|rm [-f]|restart|logs [-f] <name|id>

Run 'sitefile <command> --help' for the options of a command.
`;

export function isExtraCommand(name: string | undefined): boolean {
  return name !== undefined && EXTRA_COMMANDS.includes(name);
}

const GROUPS: ReadonlyMap<string, { subs: ReadonlyMap<string, Command>; usage: string }> = new Map([
  ['image', { subs: imageSubcommands, usage: 'image <ls|inspect|tag|rm|save|prune|diff>' }],
  ['server', { subs: serverSubcommands, usage: 'server <ls|stop|rm|logs|restart>' }],
  ['cache', { subs: cacheSubcommands, usage: 'cache <info|prune>' }],
]);

export async function runExtraCommand(argv: readonly string[]): Promise<number> {
  const [name, ...rest] = argv;
  const group = name === undefined ? undefined : GROUPS.get(name);
  if (group) {
    const [sub, ...args] = rest;
    const cmd = sub === undefined ? undefined : group.subs.get(sub);
    if (!cmd) {
      if (sub === undefined || sub === '--help' || sub === '-h') {
        const lines = [...new Set(group.subs.values())].map((c) => `  ${c.usage.padEnd(44)} ${c.summary}`);
        process.stdout.write(`Usage: sitefile ${group.usage} ...\n\n${lines.join('\n')}\n`);
        return sub === undefined ? 2 : 0;
      }
      return usageError(`unknown ${name} command '${sub}'`);
    }
    return runCommand(cmd, args);
  }
  const cmd = name === undefined ? undefined : TOP.get(name);
  if (!cmd) return usageError(`unknown command '${name ?? ''}'`);
  return runCommand(cmd, rest);
}

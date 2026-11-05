import { runCommand, usageError, type Command } from './common.js';
import { imageCommands, imageSubcommands } from './images.js';
import { registryCommands } from './registry.js';
import { serverCommands } from './servers.js';

/**
 * Image-store and site-server commands. They live outside cli.ts' build/validate/inspect
 * parser: each has its own options and `--help`.
 */
const TOP: ReadonlyMap<string, Command> = new Map([...imageCommands, ...serverCommands, ...registryCommands].filter(([k]) => !k.includes(' ')));

export const EXTRA_COMMANDS: readonly string[] = [...TOP.keys(), 'image'];

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

Servers (detached ones are tracked like containers):
  sitefile run [--name n] [-p [host:]port] [-d] [--spa] <ref|./dir>
  sitefile ps [-a] [--json]
  sitefile stop|rm [-f]|restart|logs [-f] <id|name>

Run 'sitefile <command> --help' for the options of a command.
`;

export function isExtraCommand(name: string | undefined): boolean {
  return name !== undefined && EXTRA_COMMANDS.includes(name);
}

export async function runExtraCommand(argv: readonly string[]): Promise<number> {
  const [name, ...rest] = argv;
  if (name === 'image') {
    const [sub, ...args] = rest;
    const cmd = sub === undefined ? undefined : imageSubcommands.get(sub);
    if (!cmd) {
      if (sub === undefined || sub === '--help' || sub === '-h') {
        const lines = [...new Set(imageSubcommands.values())].map((c) => `  ${c.usage.padEnd(44)} ${c.summary}`);
        process.stdout.write(`Usage: sitefile image <ls|inspect|tag|rm|save|prune|diff> ...\n\n${lines.join('\n')}\n`);
        return sub === undefined ? 2 : 0;
      }
      return usageError(`unknown image command '${sub}'`);
    }
    return runCommand(cmd, args);
  }
  const cmd = name === undefined ? undefined : TOP.get(name);
  if (!cmd) return usageError(`unknown command '${name ?? ''}'`);
  return runCommand(cmd, rest);
}

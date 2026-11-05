import { execFile } from 'node:child_process';
import { promises as fs } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { after } from 'node:test';

// lib/test/helpers/cli.js -> repo root
export const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
export const cliPath = path.join(repoRoot, 'lib', 'src', 'cli.js');

const temps: string[] = [];
after(async () => {
  for (const d of temps) await fs.rm(d, { recursive: true, force: true });
});

export async function tmpDir(prefix = 'sitefile-t-'): Promise<string> {
  const d = await fs.realpath(await fs.mkdtemp(path.join(os.tmpdir(), prefix)));
  temps.push(d);
  return d;
}

export interface CliResult {
  code: number;
  stdout: string;
  stderr: string;
}

/** Run the CLI with an isolated SITEFILE_HOME (tests must never touch ~/.sitefile). */
export function runCli(home: string, args: string[], cwd = repoRoot, env: Record<string, string> = {}): Promise<CliResult> {
  return new Promise((resolve) => {
    execFile(process.execPath, [cliPath, ...args], { cwd, env: { ...process.env, SITEFILE_HOME: home, ...env } }, (err, stdout, stderr) => {
      resolve({ code: err ? (typeof err.code === 'number' ? err.code : 1) : 0, stdout, stderr });
    });
  });
}

/** A small site context: index, a nested page, a stylesheet. */
export async function makeSite(extra: Record<string, string> = {}, sitefile?: string): Promise<string> {
  const dir = await tmpDir('sitefile-site-');
  const files: Record<string, string> = {
    'public/index.html': '<!doctype html><title>Home</title><link rel=stylesheet href=style.css>',
    'public/style.css': 'body{margin:0}',
    'public/about/index.html': '<h1>About</h1>',
    ...extra,
  };
  for (const [p, c] of Object.entries(files)) {
    await fs.mkdir(path.dirname(path.join(dir, p)), { recursive: true });
    await fs.writeFile(path.join(dir, p), c);
  }
  await fs.writeFile(path.join(dir, 'Sitefile'), sitefile ?? 'FROM scratch\nCOPY ./public/ /\nEXPORT /\n');
  return dir;
}

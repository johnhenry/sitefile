import { test } from 'node:test';
import assert from 'node:assert/strict';
import { promises as fs } from 'node:fs';
import path from 'node:path';
import { SITEFILE_VERSION } from '../src/attest.js';
import { repoRoot, runCli, tmpDir } from './helpers/cli.js';

// johnhenry/sitefile#146: package.json is the source of truth for the version. TypeScript reads it;
// the Rust port takes rust/Cargo.toml's version, so a release bump that forgets either fails here.

async function packageVersion(): Promise<string> {
  return (JSON.parse(await fs.readFile(path.join(repoRoot, 'package.json'), 'utf8')) as { version: string }).version;
}

test('version: SITEFILE_VERSION and --version are package.json version', async () => {
  const version = await packageVersion();
  assert.equal(SITEFILE_VERSION, version);
  const r = await runCli(await tmpDir(), ['--version']);
  assert.equal(r.code, 0);
  assert.equal(r.stdout, `${version}\n`);
  const help = await runCli(await tmpDir(), ['--help']);
  assert.ok(help.stdout.startsWith(`sitefile ${version} — `));
});

test('version: rust/Cargo.toml and Cargo.lock carry the package.json version', async () => {
  const version = await packageVersion();
  const toml = await fs.readFile(path.join(repoRoot, 'rust', 'Cargo.toml'), 'utf8');
  const pkg = /^\[package\]\n(?:[^[]*?\n)?version = "([^"]+)"/m.exec(toml);
  assert.equal(pkg?.[1], version, 'rust/Cargo.toml [package] version must equal package.json version');
  const lock = await fs.readFile(path.join(repoRoot, 'rust', 'Cargo.lock'), 'utf8');
  assert.match(lock, new RegExp(`\\nname = "sitefile"\\nversion = "${version.replace(/\./g, '\\.')}"\\n`), 'rust/Cargo.lock is stale: run cargo build');
});

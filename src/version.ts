import { readFileSync } from 'node:fs';

/**
 * The Sitefile version: `sitefile --version`, the help text and the builder version in build
 * attestations. package.json is the source of truth (read at load time; this file is compiled to
 * lib/src/version.js, two levels below it). The Rust port takes it from rust/Cargo.toml, which a
 * test in each implementation keeps equal to package.json (johnhenry/sitefile#146).
 */
export const SITEFILE_VERSION: string = (
  JSON.parse(readFileSync(new URL('../../package.json', import.meta.url), 'utf8')) as { version: string }
).version;

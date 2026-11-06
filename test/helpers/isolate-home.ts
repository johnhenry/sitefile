/**
 * Preloaded into every test process (`node --test --import`, see package.json):
 * give the run its own SITEFILE_HOME so no test, and no CLI process a test
 * spawns, ever reads or writes ~/.sitefile. Builds evict cache entries
 * (SITEFILE_CACHE_MAX), so a shared home would also delete the user's cache.
 */
import { mkdtempSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const real = path.resolve(os.homedir(), '.sitefile');
const given = process.env.SITEFILE_HOME;
if (given === undefined || given === '' || path.resolve(given) === real) {
  process.env.SITEFILE_HOME = mkdtempSync(path.join(os.tmpdir(), 'sitefile-test-home-'));
}

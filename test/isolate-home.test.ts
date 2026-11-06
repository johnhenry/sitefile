import { test } from 'node:test';
import assert from 'node:assert/strict';
import os from 'node:os';
import path from 'node:path';

test('the test run never uses ~/.sitefile', () => {
  const home = process.env.SITEFILE_HOME;
  assert.ok(home, 'SITEFILE_HOME is set (npm test preloads test/helpers/isolate-home)');
  assert.notEqual(path.resolve(home), path.resolve(os.homedir(), '.sitefile'));
});

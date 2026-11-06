// CRLF line endings (corpus files are LF-only, so this one is written here).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { language, treeSitter } from './helpers.mjs';

test('CRLF: instructions, continuations and heredocs', () => {
  const file = path.join(mkdtempSync(path.join(tmpdir(), 'sitefile-crlf-')), 'Sitefile');
  writeFileSync(file, ['# comment', 'FROM scratch', 'IMPORT d FROM ./d.json \\', '  AS json', 'COPY <<CSS /a.css', 'a {}', 'CSS', 'EXPORT /', ''].join('\r\n'));
  const out = treeSitter(['parse', ...language(), '--no-ranges', file]);
  assert.doesNotMatch(out, /ERROR|MISSING/);
  assert.match(out, /\(copy_instruction[\s\S]*body: \(heredoc_body\)\s*end: \(heredoc_end\)\)/);
  assert.match(out, /\(import_instruction[\s\S]*kind: \(import_kind\)/);
});

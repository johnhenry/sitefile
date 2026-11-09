// Portable paths (docs/LANGUAGE.md, "Portable paths").
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { portabilityProblems, windowsProblem } from '../src/portable.js';

test('windowsProblem', () => {
  assert.equal(windowsProblem('/a/b.html'), undefined);
  assert.equal(windowsProblem('/a:b'), "':' is not allowed in a Windows file name");
  assert.equal(windowsProblem('/x/a\u0001'), 'U+0001 is not allowed in a Windows file name');
  assert.equal(windowsProblem('/q?.txt'), "'?' is not allowed in a Windows file name");
  assert.equal(windowsProblem('/dir./a'), "'dir.' ends with a dot, which Windows drops");
  assert.equal(windowsProblem('/a '), "'a ' ends with a space, which Windows drops");
  for (const n of ['CON', 'nul.txt', 'Aux.html', 'com1', 'LPT9.tar.gz', 'COM¹.x', 'con .txt']) assert.match(windowsProblem('/d/' + n) ?? '', /reserved device name/, n);
  for (const n of ['console', 'com10', 'lpt', 'nul-x.txt', 'xcon']) assert.equal(windowsProblem('/' + n), undefined, n);
});

test('portabilityProblems: Windows names, then GitHub Pages', () => {
  assert.deepEqual(portabilityProblems(['/_headers', '/_redirects', '/index.html']), []);
  assert.deepEqual(portabilityProblems(['/.nojekyll', '/_next/a.js']), []);
  assert.deepEqual(portabilityProblems(['/_a', '/_b', '/c/_d', '/e/_f/g', '/nul']), [
    "'/nul' cannot be extracted on Windows: 'nul' is a reserved device name on Windows",
    "GitHub Pages (with Jekyll) does not publish files or folders whose name starts with '_' (/_a, /_b, /c/_d, … (4 in all)); if the site is hosted there, add an empty /.nojekyll",
  ]);
});

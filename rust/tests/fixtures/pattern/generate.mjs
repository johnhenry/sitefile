// Regenerates fuzz.json: seeded random schema patterns and strings, answered by the TypeScript engine
// (src/pattern.ts). rust/src/pattern.rs must give the same answers (`differential_fuzz`), and
// test/pattern.test.ts checks the file is current and that every accepted pattern means what
// JavaScript's RegExp (with the u flag) means.
//
//   npm run build && node rust/tests/fixtures/pattern/generate.mjs
import { writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '../../../..');
const { fuzzCases } = await import(pathToFileURL(path.join(root, 'lib/test/helpers/pattern-fuzz.js')).href);
const cases = fuzzCases(Number(process.env.FUZZ_COUNT ?? 1500), Number(process.env.FUZZ_SEED ?? 135));
writeFileSync(path.join(here, 'fuzz.json'), JSON.stringify(cases) + '\n');
console.log(`${cases.length} patterns`);

// Bundles src/ with esbuild (types are checked by `npm run typecheck`):
//   dist/extension.js       the extension, CommonJS, with vscode-languageclient
//                           inlined so the .vsix ships no node_modules
//   dist/serverCommand.mjs  the pure settings logic, for the unit tests
import { build } from 'esbuild';

const root = new URL('..', import.meta.url).pathname;
const common = { absWorkingDir: root, bundle: true, platform: 'node', target: 'node18', logLevel: 'warning' };

await build({
  ...common,
  entryPoints: ['src/extension.ts'],
  outfile: 'dist/extension.js',
  format: 'cjs',
  external: ['vscode'],
  minify: true,
});
await build({
  ...common,
  entryPoints: ['src/serverCommand.ts'],
  outfile: 'dist/serverCommand.mjs',
  format: 'esm',
});

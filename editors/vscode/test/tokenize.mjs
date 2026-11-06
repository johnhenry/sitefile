// Tokenizes Sitefile source with the committed grammar, using vscode-textmate
// (the engine VS Code itself uses). Embedded grammars are stubs: these tests
// are about our grammar, not HTML's.
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import vsctm from 'vscode-textmate';
import oniguruma from 'vscode-oniguruma';
import { EMBEDS } from '../scripts/grammar.mjs';

const require = createRequire(import.meta.url);
const syntaxes = new URL('../syntaxes/', import.meta.url);
const load = (file) => JSON.parse(readFileSync(new URL(file, syntaxes), 'utf8'));

/**
 * The HTML stub has one multi-line rule (`<` … `>`) so tests can check that an
 * unclosed construct in an embedded body cannot swallow the closing line.
 */
function stub(scopeName) {
  if (scopeName === 'text.html.basic') {
    return { scopeName, patterns: [{ name: 'meta.tag.stub', begin: '<', end: '>' }] };
  }
  return { scopeName, patterns: [] };
}

let registryPromise;
function registry() {
  registryPromise ??= (async () => {
    const wasm = readFileSync(require.resolve('vscode-oniguruma/release/onig.wasm')).buffer;
    await oniguruma.loadWASM(wasm);
    const embedded = new Set(EMBEDS.map((e) => e.scope));
    return new vsctm.Registry({
      onigLib: Promise.resolve({
        createOnigScanner: (patterns) => new oniguruma.OnigScanner(patterns),
        createOnigString: (s) => new oniguruma.OnigString(s),
      }),
      loadGrammar: async (scopeName) => {
        if (scopeName === 'source.sitefile') return load('sitefile.tmLanguage.json');
        if (scopeName === 'sitefile.template.injection') return load('sitefile-template.injection.tmLanguage.json');
        if (embedded.has(scopeName)) return stub(scopeName);
        return null;
      },
      getInjections: (scopeName) => (scopeName === 'source.sitefile' ? ['sitefile.template.injection'] : undefined),
    });
  })();
  return registryPromise;
}

/** Returns one entry per line: { line, text, tokens: [[text, scopes]] } (scopes without the root). */
export async function tokenize(source) {
  const grammar = await (await registry()).loadGrammar('source.sitefile');
  let state = vsctm.INITIAL;
  const lines = source.split(/\r?\n/);
  if (lines.length > 1 && lines[lines.length - 1] === '') lines.pop();
  return lines.map((text, i) => {
    const r = grammar.tokenizeLine(text, state);
    state = r.ruleStack;
    const tokens = r.tokens.map((t) => [text.slice(t.startIndex, t.endIndex), t.scopes.slice(1).join(' ')]);
    return { line: i + 1, text, tokens };
  });
}

/** Scopes of the first token whose text is `text` on the given (1-based) line. */
export function scopesOf(result, line, text) {
  const tok = result[line - 1]?.tokens.find(([t]) => t === text);
  return tok ? tok[1] : undefined;
}

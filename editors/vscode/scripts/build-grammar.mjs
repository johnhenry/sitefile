// Writes syntaxes/*.json from scripts/grammar.mjs.
import { writeFileSync } from 'node:fs';
import { buildGrammar, buildInjection } from './grammar.mjs';

const out = (file, value) => writeFileSync(new URL(`../syntaxes/${file}`, import.meta.url), JSON.stringify(value, null, 2) + '\n');
out('sitefile.tmLanguage.json', buildGrammar());
out('sitefile-template.injection.tmLanguage.json', buildInjection());

// What the TypeScript reference does with one Sitefile source, as plain JSON.
// Shared by generate.mjs (which writes the fixtures) and the TypeScript test
// test/parser-fuzz-fixture.test.ts (which checks they are up to date).
//
//   { input, parse, plan? }
//   parse: { ok: [instruction, …] }   the validated IR (parseProgram), spans as [line, column]
//        | { error: [code, line, column, detail] }
//   plan:  { ok: [warning, …], json }  only when parse is ok: `sitefile validate`
//                                     (json: its --json output, spans as [line, column])
//        | { error: [code, line, column, detail] }   (ARGs without a default get
//                                     the language server's placeholder values)
// A non-SitefileError exception is recorded as { crash: message } (a bug).
import path from 'node:path';
import { pathToFileURL } from 'node:url';

export async function loadLib(root) {
  const imp = (p) => import(pathToFileURL(path.join(root, 'lib/src', p)).href);
  const [{ parseSitefile }, { validate }, { plan }, { SitefileError }] = await Promise.all([
    imp('language/parser.js'),
    imp('language/validate.js'),
    imp('planner.js'),
    imp('errors.js'),
  ]);
  return { parseSitefile, validate, plan, SitefileError };
}

/** The value an ARG without a default gets (rust/src/lsp/diagnostics.rs `placeholder`). */
export function placeholder(type) {
  return type === 'number' ? '0' : type === 'boolean' ? 'false' : 'placeholder';
}

const wf = (s) => (typeof s === 'string' ? s.toWellFormed() : s);

/** IR as compact JSON: spans become [line, column] (the file is always the same). */
export function compact(value) {
  return JSON.parse(
    JSON.stringify(value, (_k, v) => {
      if (v && typeof v === 'object' && !Array.isArray(v) && 'file' in v && 'line' in v && 'column' in v && Object.keys(v).length === 3) {
        return [v.line, v.column];
      }
      return wf(v);
    }),
  );
}

function failure(lib, e) {
  if (e instanceof lib.SitefileError) return { error: [e.code, e.span?.line ?? null, e.span?.column ?? null, wf(e.detail)] };
  return { crash: wf(String(e?.stack ?? e)) };
}

export function record(lib, input, file = 'Sitefile') {
  const out = { input };
  let program;
  try {
    program = lib.validate(lib.parseSitefile(input, file), file);
  } catch (e) {
    out.parse = failure(lib, e);
    return out;
  }
  out.parse = { ok: compact(program.instructions) };
  const args = {};
  for (const ins of program.instructions) if (ins.kind === 'ARG' && !ins.defaultValue) args[ins.name] = placeholder(ins.type);
  try {
    const p = lib.plan(program, args);
    // `json`: what `sitefile validate --json` prints (src/cli.ts), spans compacted.
    const json = { file: p.file, vars: Object.fromEntries(p.vars), imports: p.imports, stages: p.stages, export: p.exportPath };
    out.plan = { ok: p.warnings.map(wf), json: compact(json) };
  } catch (e) {
    out.plan = failure(lib, e);
  }
  return out;
}

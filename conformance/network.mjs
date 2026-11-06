// Network conformance cases: case directories whose case.json has "network": true.
//
// case.json fields for a network case:
//   steps    list of CLI invocations, each an argv array or { args, env }. Every
//            implementation runs them in order and stops at the first failure.
//            Step exit codes and `error[CODE]: …` lines must match.
//   outputs  [[path, kind], …] under ${OUT} compared after the steps succeed
//            (kind: tree | archive | bytes | oci); default none. A path written
//            ctx:<path> is in the context copy instead (e.g. a vendor directory).
//   stdout   true: every step's stdout must be identical too (paths to the
//            implementation's scratch directory are replaced by ${TMP}).
//   lock     true: Sitefile.lock in the context copy must be byte-identical
//            (or absent in both) after the steps.
//   manifests  ["repo:tag", …] (placeholders allowed, e.g. conf/cache-${IMPL}:1):
//            the digests of these manifests in the mock registry after the
//            steps must be equal between implementations (bytes pushed)
//   errors   "code": compare only the error code, for errors whose text embeds
//            a third-party library's wording; default the whole line.
//
// Placeholders, substituted in args, env values and the context's text files:
//   ${HTTPS}     https://127.0.0.1:<port> of the local HTTPS server (servers.mjs)
//   ${REGISTRY}  127.0.0.1:<port> of the mock registry
//   ${CTX}       this implementation's copy of the case directory
//   ${OUT}       this implementation's output directory
//   ${TMP}       this implementation's scratch directory
//   ${IMPL}      ts or rust
// Only ${HTTPS} and ${REGISTRY} are replaced inside context files (templates
// use ${…} for their own variables).
//
// Each implementation gets its own context copy, SITEFILE_HOME, HOME and
// DOCKER_CONFIG under its scratch directory, so lockfiles, image stores and
// credentials never interfere with each other or with the user's.

import { promises as fs } from 'node:fs';
import path from 'node:path';

const FILE_VARS = ['HTTPS', 'REGISTRY'];

function substitute(s, vars, names = Object.keys(vars)) {
  let out = s;
  for (const k of names) out = out.split('${' + k + '}').join(vars[k]);
  return out;
}

async function copyContext(from, to, vars) {
  await fs.mkdir(to, { recursive: true });
  for (const e of await fs.readdir(from, { withFileTypes: true })) {
    if (e.name === 'case.json') continue;
    const src = path.join(from, e.name);
    const dst = path.join(to, e.name);
    if (e.isDirectory()) await copyContext(src, dst, vars);
    else {
      const bytes = await fs.readFile(src);
      const text = bytes.toString('utf8');
      const isText = Buffer.from(text, 'utf8').equals(bytes);
      await fs.writeFile(dst, isText ? substitute(text, vars, FILE_VARS) : bytes);
      await fs.chmod(dst, (await fs.stat(src)).mode & 0o777);
    }
  }
}

const readOrNull = (p) => fs.readFile(p, 'utf8').catch((e) => (e.code === 'ENOENT' ? null : Promise.reject(e)));

/**
 * Run one network case with both implementations.
 * ctx: { impls: [[name, cmd]], tmp, cwd (the CLIs' working directory), servers, run, outputOf, errorLine }
 */
export async function runNetworkCase(c, ctx) {
  const r = {};
  const codeOf = (line) => /^error\[([A-Z_]+)\]/.exec(line)?.[1] ?? line;
  for (const [impl, cmd] of ctx.impls) {
    const base = path.join(ctx.tmp, impl, c.name.replace(/[/:]/g, '_'));
    const vars = { ...ctx.servers.vars, CTX: path.join(base, 'ctx'), OUT: path.join(base, 'out'), TMP: base, IMPL: impl };
    await fs.mkdir(vars.OUT, { recursive: true });
    await copyContext(c.context, vars.CTX, vars);
    const env = {
      ...ctx.servers.env,
      SITEFILE_HOME: path.join(base, 'home'),
      HOME: path.join(base, 'user'),
      DOCKER_CONFIG: path.join(base, 'docker'),
      SITEFILE_REGISTRY_AUTH: undefined,
      SITEFILE_INSECURE_REGISTRIES: undefined,
    };
    const steps = [];
    let ms = 0;
    let spawnError;
    for (const step of c.steps ?? []) {
      const { args, env: stepEnv = {} } = Array.isArray(step) ? { args: step } : step;
      const argv = args.map((a) => substitute(a, vars));
      const e = { ...env, ...Object.fromEntries(Object.entries(stepEnv).map(([k, v]) => [k, substitute(v, vars)])) };
      const res = await ctx.run(cmd, argv, e);
      ms += res.ms;
      spawnError ??= res.spawnError;
      // Paths to this implementation's scratch directory (absolute, or relative to
      // the CLIs' working directory) differ between the two runs.
      const scrub = (s) => s.split(path.relative(ctx.cwd, vars.TMP)).join('${TMP}').split(vars.TMP).join('${TMP}');
      const line = scrub(ctx.errorLine(res.stderr));
      steps.push({ command: argv[0], code: res.code, error: c.errors === 'code' ? codeOf(line) : line, ...(c.stdout ? { stdout: scrub(res.stdout) } : {}) });
      if (res.code !== 0) break;
    }
    const last = steps[steps.length - 1] ?? { code: 0, error: '' };
    const out = { code: last.code, error: last.error, ms, spawnError, steps };
    if (steps.every((s) => s.code === 0)) {
      const parts = [];
      for (const [p, kind] of c.outputs ?? []) {
        const where = p.startsWith('ctx:') ? path.join(vars.CTX, p.slice(4)) : path.join(vars.OUT, p);
        const o = await ctx.outputOf(kind, where).catch((err) => ({ digest: `unreadable: ${err.message}` }));
        parts.push(`${p}=${o.digest}`);
      }
      out.output = { digest: parts.join(' | '), files: parts.length };
    }
    if (c.lock) out.lock = await readOrNull(path.join(vars.CTX, 'Sitefile.lock'));
    if (c.manifests) out.manifests = c.manifests.map((k) => `${k.split('${IMPL}').join('*')}=${ctx.servers.manifestDigest(substitute(k, vars))}`).join(' | ');
    r[impl] = out;
  }
  const [a, b] = [r.ts, r.rust];
  const stepsSame = JSON.stringify(a.steps.map((s) => [s.code, s.error, s.stdout])) === JSON.stringify(b.steps.map((s) => [s.code, s.error, s.stdout]));
  const pass = !b.spawnError && stepsSame && a.output?.digest === b.output?.digest && (!c.lock || a.lock === b.lock) && a.manifests === b.manifests && !(a.manifests ?? "").includes("=null");
  const detail = [];
  if (!pass) {
    for (const impl of ['ts', 'rust']) {
      for (const s of r[impl].steps) detail.push(`${impl.padEnd(4)} ${s.command}: exit ${s.code} ${s.error}${s.stdout !== undefined ? `\n${s.stdout}` : ''}`);
      if (r[impl].output) detail.push(`${impl.padEnd(4)} outputs ${r[impl].output.digest}`);
      if (r[impl].manifests) detail.push(`${impl.padEnd(4)} manifests ${r[impl].manifests}`);
    }
    if (c.lock && a.lock !== b.lock) detail.push(`Sitefile.lock differs:\n--- ts\n${a.lock ?? '(none)'}\n--- rust\n${b.lock ?? '(none)'}`);
  }
  return { name: c.name, compare: 'network', pass, ts: a, rust: b, detail };
}

import { statSync } from 'node:fs';
import path from 'node:path';
import { SitefileError } from './errors.js';
import { joinVirtual, normalizeVirtual } from './paths.js';
import { sidecarSource } from './precompress.js';
import { importedStageRefs, type Operation, type Plan, type SourceRef, type Stage } from './planner.js';
import type { BuildResult, ImportedSitefile } from './executor.js';
import type { FileNode, Tree } from './tree.js';
import type { Resource } from './resources/types.js';
import { importedStage } from './language/ast.js';

// `sitefile graph` and `sitefile why` (issue #94): the dependency graph of a plan, and the history of one
// output file. Both outputs are part of the CLI contract and identical in the Rust port (rust/src/explain.rs).

// ---------------------------------------------------------------- labels

/** A context-relative path as written in a Sitefile: `./src`, `./` for the context root. */
function localLabel(p: string): string {
  return './' + p;
}

/** A source as an operation label shows it. */
function sourceLabel(ref: SourceRef): string {
  switch (ref.kind) {
    case 'inline':
      return `<<${ref.delimiter}`;
    case 'stage':
      return `--from=${ref.name} ${ref.path}`;
    case 'local':
      return localLabel(ref.path);
    case 'import':
      return ref.path === '/' ? `${ref.name}:` : `${ref.name}:${ref.path}`;
  }
}

/** One line describing an operation: its keyword, source and destination (FOR destinations are not evaluated). */
export function operationLabel(op: Operation): string {
  switch (op.kind) {
    case 'COPY':
      return `COPY ${sourceLabel(op.source)} ${op.dest}${op.destIsDir && op.dest !== '/' ? '/' : ''}`;
    case 'RENDER':
      return op.each
        ? `RENDER ${sourceLabel(op.template)} FOR ${op.each.as} IN ${op.each.list}${op.each.by !== undefined ? ` BY ${op.each.by}` : ''}${op.each.page ? ` PAGE ${op.each.page.size}` : ''}`
        : `RENDER ${sourceLabel(op.template)} TO ${op.dest}`;
    case 'TRANSFORM':
      return `TRANSFORM ${op.plugin}${op.source ? ` ${sourceLabel(op.source)}` : ''}${op.each ? ` FOR ${op.each.as} IN ${op.each.list}` : ''}`;
    case 'SITEMAP':
      return `SITEMAP ${op.dest}`;
    case 'FEED':
      return op.each ? `FEED FOR ${op.each.as} IN ${op.list} BY ${op.each.by}` : `FEED ${op.list} ${op.dest}`;
    case 'FINGERPRINT':
      return `FINGERPRINT ${op.patterns.join(' ')}`;
    case 'CANONICAL':
      return `CANONICAL ${op.base}${op.pretty ? ' PRETTY' : ''}`;
    case 'PRECOMPRESS':
      return `PRECOMPRESS ${op.formats.join(' ')} ${op.patterns.join(' ')}`;
    case 'ROBOTS':
    case 'SECURITY_TXT':
      return `${op.kind} ${op.dest}`;
    case 'PRECACHE':
      return `PRECACHE ${op.patterns.join(' ')}`;
    case 'CHECKLINKS':
      return `CHECKLINKS${op.warn ? ' --warn' : ''}${op.external ? ' --external' : ''}`;
  }
}

function stageLabel(stage: Stage): string {
  const b = stage.base;
  const base =
    b.kind === 'scratch' ? 'scratch' : b.kind === 'local' ? localLabel(b.path) : b.kind === 'image' ? `image ${b.ref}${b.at !== undefined ? ` AT ${b.at}` : ''}` : b.name;
  return `FROM ${base}${stage.name !== undefined ? ` AS ${stage.name}` : ''}`;
}

/** `build`, or `#2` for the second stage when it has no name. */
function stageDisplay(stage: Stage, index: number): string {
  return stage.name ?? `#${index + 1}`;
}

function stageId(stage: Stage, index: number): string {
  return `stage:${stageDisplay(stage, index)}`;
}

// ---------------------------------------------------------------- graph

export type GraphNodeKind = 'import' | 'image' | 'plugin' | 'tool' | 'local' | 'operation' | 'stage' | 'site' | 'export';
export type GraphEdgeKind = 'base' | 'input' | 'next' | 'export';

export interface GraphNode {
  readonly id: string;
  readonly kind: GraphNodeKind;
  readonly label: string;
  readonly line?: number;
}

export interface GraphEdge {
  readonly from: string;
  readonly to: string;
  readonly kind: GraphEdgeKind;
}

export interface Graph {
  readonly file: string;
  readonly target?: string;
  readonly nodes: readonly GraphNode[];
  readonly edges: readonly GraphEdge[];
}

/**
 * The plan as a graph. Nodes: imports, plugins, tools, local paths read, operations (`op:<line>`), stages (their
 * final trees), the EXPORT site files and the export. Edges point from what is read to what reads it: `base` (a
 * stage's starting tree to its first operation), `next` (an operation's tree to the following operation, the
 * last one to its stage), `input` (sources, plugins, tools and FOR/FEED lists) and `export`.
 */
export function planGraph(p: Plan): Graph {
  const nodes: GraphNode[] = [];
  const locals: GraphNode[] = [];
  const edges: GraphEdge[] = [];
  const seen = new Set<string>();
  const edge = (from: string, to: string, kind: GraphEdgeKind): void => {
    const key = `${from}\0${to}\0${kind}`;
    if (seen.has(key)) return;
    seen.add(key);
    edges.push({ from, to, kind });
  };
  const local = (rel: string): string => {
    const id = `local:${localLabel(rel)}`;
    if (!locals.some((n) => n.id === id)) locals.push({ id, kind: 'local', label: localLabel(rel) });
    return id;
  };
  const imports = new Set(p.imports.map((i) => i.name));
  const tools = new Set(p.tools.map((t) => t.name));
  const stageIds = new Map<string, string>();
  const sourceNode = (ref: SourceRef): string | undefined => {
    switch (ref.kind) {
      case 'inline':
        return undefined;
      case 'stage':
        return stageIds.get(ref.name);
      case 'local':
        return local(ref.path);
      case 'import':
        return `import:${ref.name}`;
    }
  };
  const list = (name: string): string | undefined => (imports.has(name) ? `import:${name}` : undefined);

  // An IMPORT … SCHEMA reads its schema file.
  for (const i of p.imports) if (i.schema) edge(local(i.schema.path), `import:${i.name}`, 'input');

  // Stages of imported Sitefiles (IMPORT … AS sitefile) are read like stages: one node each, fed by the import.
  const importedStages: GraphNode[] = [];
  for (const ref of importedStageRefs(p.stages)) {
    const id = `stage:${ref.name}`;
    if (stageIds.has(ref.name)) continue;
    const line = p.imports.find((i) => i.name === ref.import)?.span.line;
    importedStages.push({ id, kind: 'stage', label: `stage ${ref.stage} of IMPORT ${ref.import}`, ...(line !== undefined ? { line } : {}) });
    stageIds.set(ref.name, id);
    edge(`import:${ref.import}`, id, 'input');
  }
  const staged: GraphNode[] = [];
  const images: GraphNode[] = [];
  p.stages.forEach((stage, si) => {
    const id = stageId(stage, si);
    const ids = stage.operations.map((op) => `op:${op.span.line}`);
    const first = ids[0] ?? id;
    if (stage.base.kind === 'local') edge(local(stage.base.path), first, 'base');
    else if (stage.base.kind === 'image') {
      const imageId = `image:${stage.base.ref}`;
      if (!images.some((n) => n.id === imageId)) images.push({ id: imageId, kind: 'image', label: `image ${stage.base.ref}`, line: stage.span.line });
      edge(imageId, first, 'base');
    }
    else if (stage.base.kind === 'stage') {
      const from = stageIds.get(stage.base.name);
      if (from) edge(from, first, 'base');
    }
    stage.operations.forEach((op, i) => {
      const opId = ids[i]!;
      staged.push({ id: opId, kind: 'operation', label: operationLabel(op), line: op.span.line });
      const inputs: Array<string | undefined> = [];
      if (op.kind === 'COPY') inputs.push(sourceNode(op.source));
      if (op.kind === 'RENDER') inputs.push(sourceNode(op.template), op.each ? list(op.each.list) : undefined);
      if (op.kind === 'TRANSFORM') {
        inputs.push(op.source ? sourceNode(op.source) : undefined, `${tools.has(op.plugin) ? 'tool' : 'plugin'}:${op.plugin}`, op.each ? list(op.each.list) : undefined);
      }
      if (op.kind === 'FEED') inputs.push(list(op.list));
      for (const from of inputs) if (from !== undefined) edge(from, opId, 'input');
      edge(opId, ids[i + 1] ?? id, 'next');
    });
    staged.push({ id, kind: 'stage', label: stageLabel(stage), line: stage.span.line });
    if (stage.name !== undefined) stageIds.set(stage.name, id);
  });

  const last = p.stages[p.stages.length - 1]!;
  edge(stageId(last, p.stages.length - 1), 'export', 'export');
  const site: GraphNode[] = [];
  for (const [name, rules, kind] of [
    ['_redirects', p.redirects, 'REDIRECT'],
    ['_headers', p.headers, 'HEADER'],
  ] as const) {
    if (!rules.length) continue;
    const id = `site:${name}`;
    site.push({ id, kind: 'site', label: `${joinVirtual(p.exportPath, name)} (${rules.length} ${kind} line${rules.length === 1 ? '' : 's'})`, line: rules[0]!.span.line });
    edge(id, 'export', 'export');
  }
  nodes.push(
    ...p.imports.map((i): GraphNode => ({ id: `import:${i.name}`, kind: 'import', label: `IMPORT ${i.name} ${i.uri}${i.as ? ` AS ${i.as}` : ''}`, line: i.span.line })),
    ...p.plugins.map((pl): GraphNode => ({ id: `plugin:${pl.name}`, kind: 'plugin', label: `PLUGIN ${pl.name} ${localLabel(pl.path)}`, line: pl.span.line })),
    ...p.tools.map((t): GraphNode => ({ id: `tool:${t.name}`, kind: 'tool', label: `TOOL ${t.name}`, line: t.span.line })),
    ...images,
    ...importedStages,
    ...locals,
    ...staged,
    ...site,
    { id: 'export', kind: 'export', label: `EXPORT ${p.exportPath} AS ${p.exportFormat}${p.target !== undefined ? ` (--target ${p.target})` : ''}` },
  );
  return { file: p.file, ...(p.target !== undefined ? { target: p.target } : {}), nodes, edges };
}

/** `id  label` per node, then one `  <- from (kind)` line per incoming edge, then a count. */
export function graphText(g: Graph): string {
  let out = '';
  for (const n of g.nodes) {
    out += `${n.id}  ${n.label}\n`;
    for (const e of g.edges) if (e.to === n.id) out += `  <- ${e.from} (${e.kind})\n`;
  }
  return out + `${g.nodes.length} nodes, ${g.edges.length} edges\n`;
}

const DOT_SHAPES: Record<GraphNodeKind, string> = {
  import: 'cylinder',
  image: 'tab',
  plugin: 'component',
  tool: 'component',
  local: 'folder',
  operation: 'box',
  stage: 'box3d',
  site: 'note',
  export: 'doubleoctagon',
};

function dotString(s: string): string {
  return `"${s.replace(/\\/g, '\\\\').replace(/"/g, '\\"')}"`;
}

/** Graphviz DOT (`sitefile graph --dot | dot -Tsvg > graph.svg`). */
export function graphDot(g: Graph): string {
  let out = 'digraph sitefile {\n  rankdir=LR;\n  node [shape=box];\n';
  for (const n of g.nodes) out += `  ${dotString(n.id)} [label=${dotString(n.label)}, shape=${DOT_SHAPES[n.kind]}];\n`;
  for (const e of g.edges) {
    const style = e.kind === 'input' ? ' [style=dashed]' : e.kind === 'next' ? '' : ' [style=bold]';
    out += `  ${dotString(e.from)} -> ${dotString(e.to)}${style};\n`;
  }
  return out + '}\n';
}

// ---------------------------------------------------------------- why

export interface WhyEvent {
  /** `Sitefile:12`: the instruction's line. */
  readonly location: string;
  /** The instruction, as `graph` labels it. */
  readonly instruction: string;
  /** Stage name, or `#n` for an unnamed stage. */
  readonly stage: string;
  /** The path in that stage's tree. */
  readonly path: string;
  readonly action: 'write' | 'overwrite';
  readonly digest: string;
  /** What the content came from: `./src/a.html`, `stage build:/out/a.html`, `import site:/a.html`, `<<EOF`, `plugin x`, `list posts`. */
  readonly inputs: readonly string[];
}

export interface WhyReport {
  readonly path: string;
  readonly mode: string;
  readonly size: number;
  readonly digest: string;
  readonly origin: string;
  /** Oldest first; the last event wrote the file as exported. */
  readonly history: readonly WhyEvent[];
}

function same(a: FileNode | undefined, b: FileNode | undefined): boolean {
  return a === b || (a !== undefined && b !== undefined && a.origin === b.origin && a.digest === b.digest && a.mode === b.mode);
}

/** `rel` of `full` below directory `dir` (`/x` for `/d/x` under `/d`). */
function below(dir: string, full: string): string {
  return dir === '/' ? full : full.slice(dir.length);
}

/** `base` + `rel` where `base` is a virtual path (`/` for the root) and `rel` starts with `/`. */
function under(base: string, rel: string): string {
  return base === '/' ? rel : base + rel;
}

/**
 * Explain one exported file: every write to it (and to the files it was copied from in earlier stages) that
 * changed it, oldest first. Writes of identical content are no-ops in the tree (the first writer stays the
 * owner), so they are not listed. `result` must come from a build with `recordSteps`.
 */
export function why(result: BuildResult, raw: string, contextDir: string): WhyReport {
  const p = result.plan;
  const steps = result.steps;
  if (!steps) throw new Error('why() needs a build with recordSteps');
  const want = normalizeVirtual(raw.startsWith('/') ? raw : '/' + raw, undefined, 'path').path;
  const node = result.output.get(want);
  if (!node) {
    if (result.output.isDirectory(want)) throw new SitefileError('E_NOT_FOUND', `'${want}' is a directory in the build output; name a file`);
    throw new SitefileError('E_NOT_FOUND', `'${want}' is not in the build output`);
  }
  const root: WhyScope = {
    plan: p,
    steps,
    prefix: '',
    resource: (name) => result.resources.get(name),
    isDir: (rel) => {
      try {
        return statSync(path.join(contextDir, rel)).isDirectory();
      } catch {
        return false; // gone since the build: show the path as written
      }
    },
    imported: result.imported ?? new Map(),
    ...(result.renames ? { renames: result.renames } : {}),
  };
  const scopes = new Map<ImportedSitefile, WhyScope>();
  /** The scope of a Sitefile imported by `scope` (its own messages, steps and context). */
  const scopeOf = (unit: ImportedSitefile, prefix: string): WhyScope => {
    let s = scopes.get(unit);
    if (!s) {
      s = {
        plan: unit.plan,
        steps: unit.steps ?? [],
        prefix,
        resource: (name) => unit.resources.get(name),
        isDir: (rel) => unit.isDirectory(rel),
        imported: unit.imported,
        renames: unit.renames,
      };
      scopes.set(unit, s);
    }
    return s;
  };
  /** The scope and index of the stage `name` (`build`, or `theme+assets` for a stage of an imported Sitefile). */
  const locate = (scope: WhyScope, name: string): { scope: WhyScope; index: number } => {
    const imp = importedStage(name);
    const unit = imp ? scope.imported.get(imp.import) : undefined;
    if (imp && unit) {
      const s = scopeOf(unit, `${scope.prefix}${imp.import}+`);
      return { scope: s, index: unit.plan.stages.findIndex((st) => st.name === imp.stage) };
    }
    return { scope, index: scope.plan.stages.findIndex((st) => st.name === name) };
  };
  const finalOf = (scope: WhyScope, i: number): Tree => scope.steps[i]![scope.steps[i]!.length - 1]!;

  /** Display and (for a stage) the file a source contributes to `file` written at `dest`. */
  const copied = (scope: WhyScope, ref: SourceRef, dest: string, file: string): { input: string; from?: { scope: WhyScope; stage: number; path: string } } => {
    const rel = file === dest ? undefined : below(dest, file);
    switch (ref.kind) {
      case 'inline':
        return { input: `<<${ref.delimiter}` };
      case 'local': {
        const isDir = scope.isDir(ref.path);
        const rest = isDir && rel !== undefined ? (ref.path === '' ? rel.slice(1) : ref.path + rel) : ref.path;
        return { input: localLabel(rest) };
      }
      case 'import': {
        const res = scope.resource(ref.name);
        if (res?.kind !== 'tree') return { input: ref.path === '/' ? `import ${ref.name}` : `import ${ref.name}:${ref.path}` };
        const inner = res.tree.isFile(ref.path) || rel === undefined ? ref.path : under(ref.path, rel);
        return { input: `import ${ref.name}:${inner}` };
      }
      case 'stage': {
        const loc = locate(scope, ref.name);
        const tree = finalOf(loc.scope, loc.index);
        const inner = tree.isFile(ref.path) || rel === undefined ? ref.path : under(ref.path, rel);
        return { input: `stage ${ref.name}:${inner}`, ...(tree.isFile(inner) ? { from: { scope: loc.scope, stage: loc.index, path: inner } } : {}) };
      }
    }
  };
  const readLabel = (ref: SourceRef): string => {
    switch (ref.kind) {
      case 'inline':
        return `<<${ref.delimiter}`;
      case 'local':
        return localLabel(ref.path);
      case 'import':
        return ref.path === '/' ? `import ${ref.name}` : `import ${ref.name}:${ref.path}`;
      case 'stage':
        return `stage ${ref.name}:${ref.path}`;
    }
  };

  /** The path `file` had before operation `upTo` of stage `si` when a FINGERPRINT there renamed it, and where. */
  const renamedAt = (scope: WhyScope, si: number, file: string, upTo: number): { index: number; from: string } | undefined => {
    const ops = scope.plan.stages[si]!.operations;
    for (let i = upTo - 1; i >= 0; i--) {
      const op = ops[i]!;
      if (op.kind !== 'FINGERPRINT') continue;
      const hit = scope.renames?.get(op.span.line)?.find(([, to]) => to === file);
      if (hit) return { index: i, from: hit[0] };
    }
    return undefined;
  };

  const history = (scope: WhyScope, si: number, file: string, upTo = scope.plan.stages[si]!.operations.length): WhyEvent[] => {
    const sp = scope.plan;
    const at = (line: number): string => `${sp.file}:${line}`;
    const stage = sp.stages[si]!;
    const trees = scope.steps[si]!;
    const display = scope.prefix + stageDisplay(stage, si);
    // A renamed file: the history of its old path up to the FINGERPRINT, then the rename.
    const rename = renamedAt(scope, si, file, upTo);
    if (rename) {
      const op = stage.operations[rename.index]!;
      const node = trees[rename.index + 1]!.get(file)!;
      const events = history(scope, si, rename.from, rename.index);
      events.push({ location: at(op.span.line), instruction: operationLabel(op), stage: display, path: file, action: 'write', digest: node.digest, inputs: [`${rename.from} (renamed)`] });
      return [...events, ...later(scope, si, file, rename.index + 1, upTo, node)];
    }
    const events: WhyEvent[] = [];
    const prev = trees[0]!.get(file);
    if (prev) {
      if (stage.base.kind === 'stage') {
        const loc = locate(scope, stage.base.name);
        events.push(...history(loc.scope, loc.index, file));
      } else if (stage.base.kind === 'local') {
        const rest = stage.base.path === '' ? file.slice(1) : stage.base.path + file;
        events.push({ location: at(stage.span.line), instruction: stageLabel(stage), stage: display, path: file, action: 'write', digest: prev.digest, inputs: [localLabel(rest)] });
      } else if (stage.base.kind === 'image') {
        // The file's origin names the image, its digest and the path inside the layer.
        events.push({ location: at(stage.span.line), instruction: stageLabel(stage), stage: display, path: file, action: 'write', digest: prev.digest, inputs: [prev.origin] });
      }
    }
    return [...events, ...later(scope, si, file, 0, upTo, prev)];
  };

  /** The events of operations `from` to `upTo` (exclusive) of stage `si` that changed `file` (`prev`: before them). */
  const later = (scope: WhyScope, si: number, file: string, from: number, upTo: number, prev: FileNode | undefined): WhyEvent[] => {
    const sp = scope.plan;
    const at = (line: number): string => `${sp.file}:${line}`;
    const tools = new Set(sp.tools.map((t) => t.name));
    const stage = sp.stages[si]!;
    const trees = scope.steps[si]!;
    const display = scope.prefix + stageDisplay(stage, si);
    const events: WhyEvent[] = [];
    stage.operations.slice(from, upTo).forEach((op, k) => {
      const i = from + k;
      const next = trees[i + 1]!.get(file);
      if (!next || same(prev, next)) {
        prev = next;
        return;
      }
      const inputs: string[] = [];
      if (op.kind === 'COPY') {
        const c = copied(scope, op.source, op.dest, file);
        if (c.from) events.push(...history(c.from.scope, c.from.stage, c.from.path));
        inputs.push(c.input);
      } else if (op.kind === 'RENDER') {
        inputs.push(readLabel(op.template));
        if (op.each) inputs.push(`list ${op.each.list}`);
      } else if (op.kind === 'TRANSFORM') {
        // A directory TRANSFORM names the input file in each output's origin.
        const perFile = `${at(op.span.line)} TRANSFORM `;
        if (op.source) inputs.push(next.origin.startsWith(perFile) ? next.origin.slice(perFile.length) : readLabel(op.source));
        inputs.push(`${tools.has(op.plugin) ? 'tool' : 'plugin'} ${op.plugin}`);
        if (op.each) inputs.push(`list ${op.each.list}`);
      } else if (op.kind === 'SITEMAP') inputs.push('the .html files in the tree');
      else if (op.kind === 'CHECKLINKS') return;
      else if (op.kind === 'CANONICAL') inputs.push('the page (canonical link)');
      else if (op.kind === 'ROBOTS' || op.kind === 'SECURITY_TXT') inputs.push('the Sitefile');
      else if (op.kind === 'PRECACHE') inputs.push(op.into === file ? 'the service worker (manifest injected)' : 'the files it lists');
      else if (op.kind === 'FINGERPRINT') inputs.push(op.manifest === file ? 'the files it renamed' : 'references to renamed files');
      else if (op.kind === 'PRECOMPRESS') inputs.push(`${sidecarSource(file)} (compressed)`);
      else inputs.push(`list ${op.list}`);
      events.push({ location: at(op.span.line), instruction: operationLabel(op), stage: display, path: file, action: prev ? 'overwrite' : 'write', digest: next.digest, inputs });
      prev = next;
    });
    return events;
  };


  const at = (line: number): string => `${p.file}:${line}`;
  const lastIndex = p.stages.length - 1;
  const full = under(p.exportPath, want);
  const events = history(root, lastIndex, full);
  // The EXPORT site files are added after the last stage.
  const before = finalOf(root, lastIndex).get(full);
  const after = result.tree.get(full);
  if (after && !same(before, after)) {
    const site = full === joinVirtual(p.exportPath, '_redirects') || full === joinVirtual(p.exportPath, '_headers');
    const [rules, kind] = full === joinVirtual(p.exportPath, '_redirects') ? [p.redirects, 'REDIRECT'] : [p.headers, 'HEADER'];
    const inputs = site ? rules.map((r) => `${at(r.span.line)} ${('instruction' in r && r.instruction) || kind}`) : [];
    const text = new TextDecoder().decode(after.bytes);
    // `/_headers` may also hold FINGERPRINT's Cache-Control lines (or only them): with cacheHeaders set, no
    // HEADER sets Cache-Control, so the name in the file means they were added. Likewise CSP's
    // Content-Security-Policy lines; with CSP … META, a page changed at EXPORT got its <meta>.
    const fp = site && kind === 'HEADER' && p.cacheHeaders && text.includes('  Cache-Control: ') ? p.cacheHeaders : undefined;
    if (fp) inputs.push(`${at(fp.span.line)} FINGERPRINT`);
    const csp = p.csp && (site ? kind === 'HEADER' && text.includes('  Content-Security-Policy: ') : true) ? p.csp : undefined;
    if (csp) inputs.push(site ? `${at(csp.span.line)} CSP` : 'the page (inline scripts and styles)');
    const first = site ? (rules[0] ?? fp ?? csp) : csp;
    events.push({
      location: at(first!.span.line),
      instruction: site && rules[0] ? ('instruction' in rules[0] && rules[0].instruction) || kind : site && fp ? 'FINGERPRINT' : 'CSP',
      stage: stageDisplay(p.stages[lastIndex]!, lastIndex),
      path: full,
      action: before ? 'overwrite' : 'write',
      digest: after.digest,
      inputs,
    });
  }
  return { path: want, mode: node.mode.toString(8), size: node.bytes.byteLength, digest: node.digest, origin: node.origin, history: events };
}

/** One Sitefile's view for `why`: the build's own, or one imported AS sitefile (stages shown as `theme+<stage>`). */
interface WhyScope {
  readonly plan: Plan;
  readonly steps: readonly (readonly Tree[])[];
  /** `theme+` for a Sitefile imported as `theme` ('' for the build's own). */
  readonly prefix: string;
  readonly resource: (name: string) => Resource | undefined;
  /** Whether a context-relative path is a directory (a COPY of a directory lists the file inside it). */
  readonly isDir: (rel: string) => boolean;
  readonly imported: ReadonlyMap<string, ImportedSitefile>;
  /** What each FINGERPRINT of this Sitefile renamed (old path, new path), by its line. */
  readonly renames?: ReadonlyMap<number, ReadonlyArray<readonly [string, string]>>;
}

export function whyText(r: WhyReport): string {
  let out = `${r.path}  ${r.mode}  ${r.size} bytes  ${r.digest}\n`;
  r.history.forEach((e, i) => {
    out += `  ${i + 1}. ${e.location} ${e.instruction}  [stage ${e.stage}]  ${e.action === 'write' ? 'wrote' : 'overwrote'} ${e.path}\n`;
    for (const input of e.inputs) out += `       from ${input}\n`;
  });
  return out;
}

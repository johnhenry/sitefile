// Lint rules for `sitefile validate` and the language server (docs/LANGUAGE.md, "Lint").
//
// Each rule has a stable id and reports a likely mistake in a Sitefile that is otherwise valid.
// Rules look at the Sitefile only (no IO): the validated program, its plan, and its comments.
// A warning is `<file>:<line>: [<id>] <message>`, the line being the instruction it is about.
//
// Disabling: `# sitefile-lint: disable=<id>[,<id>…]` anywhere turns rules off for the whole file,
// `# sitefile-lint: disable-next-line=<id>[,…]` for the next instruction only, and the CLI's
// `--lint-disable <id>[,<id>…]` for one run; `all` names every rule.
import type { InlineBody, Instruction, Operand, Program } from './language/ast.js';
import { isInline } from './language/ast.js';
import { logicalLines, type CommentLine, type LogicalLine } from './language/scanner.js';
import { operandsOf } from './language/validate.js';
import { templateNames } from './executor.js';
import type { Operation, Plan } from './planner.js';
import { parseRenderTemplate } from './render.js';
import { parseRef } from './ref.js';

/** Every rule, in the order warnings of one line are reported. */
export const LINT_RULES = ['unused-name', 'unpinned-import', 'unpinned-image', 'render-escape', 'sitemap-order', 'shadowed-write'] as const;
export type LintRule = (typeof LINT_RULES)[number];

/** Import kinds a RENDER template can read (so a template file may use them). */
const VALUE_KINDS = new Set(['text', 'json', 'yaml', 'csv', 'markdown', 'wbn']);

/** `# sitefile-lint: …` (name ASCII case-insensitive): group 1 is the rest of the comment. */
const LINT_COMMENT = /^#[ \t]*[Ss][Ii][Tt][Ee][Ff][Ii][Ll][Ee]-[Ll][Ii][Nn][Tt][ \t]*:([\s\S]*)$/;
const LINT_BODY = /^(disable|disable-next-line)[ \t]*=[ \t]*([\s\S]+)$/;

interface Found {
  readonly line: number;
  readonly rule: LintRule | 'lint-comment';
  readonly message: string;
}

/** Text of an operand as written (`${…}` kept), for messages and extension checks. */
function written(op: Operand): string {
  return op.parts.map((p) => (p.kind === 'text' ? p.value : `\${${[p.name, ...p.path].join('.')}}`)).join('');
}

/** Every operand of an instruction, EXPORT's BASE and AT included. */
function operands(ins: Instruction): Operand[] {
  const ops = operandsOf(ins);
  if (ins.kind === 'EXPORT') ops.push(...(ins.base ? [ins.base] : []), ...(ins.at ? [ins.at] : []));
  return ops;
}

/** Parse `# sitefile-lint:` comments: rules off for the file, rules off per line, and problems. */
function directives(comments: readonly CommentLine[], lines: readonly LogicalLine[], found: Found[]): { file: Set<string>; lines: Map<number, Set<string>> } {
  const file = new Set<string>();
  const perLine = new Map<number, Set<string>>();
  for (const c of comments) {
    const m = LINT_COMMENT.exec(c.text);
    if (!m) continue;
    const body = LINT_BODY.exec(m[1]!.trim());
    if (!body) {
      found.push({ line: c.line, rule: 'lint-comment', message: 'unrecognized # sitefile-lint comment (expected disable=<rule>[,<rule>…] or disable-next-line=<rule>[,<rule>…])' });
      continue;
    }
    const ids = body[2]!.split(',').map((s) => s.trim());
    const known = ids.filter((id) => {
      if (id === 'all' || (LINT_RULES as readonly string[]).includes(id)) return true;
      found.push({ line: c.line, rule: 'lint-comment', message: `unknown lint rule '${id}' (rules: ${LINT_RULES.join(', ')}, or all)` });
      return false;
    });
    if (body[1] === 'disable') {
      known.forEach((id) => file.add(id));
    } else {
      // The next instruction: the first logical line that starts after the comment.
      const next = lines.find((l) => (l.positions[0]?.line ?? 0) > c.line);
      if (!next) continue;
      const at = next.positions[0]!.line;
      const set = perLine.get(at) ?? new Set<string>();
      known.forEach((id) => set.add(id));
      perLine.set(at, set);
    }
  }
  return { file, lines: perLine };
}

/** Whether a RENDER line sets `--escape=` explicitly (before its template). */
function explicitEscape(line: LogicalLine | undefined): boolean {
  if (!line) return false;
  const words = line.text.split(/\s+/).filter((w) => w !== '');
  for (const w of words.slice(1)) {
    if (!w.startsWith('--')) break;
    if (w.startsWith('--escape=')) return true;
  }
  return false;
}

/** The file path a RENDER or COPY writes, when it is known before the build (otherwise undefined). */
function writtenPath(op: Operation): string | undefined {
  if (op.kind === 'RENDER') return op.each ? written(op.each.dest) : op.dest;
  if (op.kind === 'COPY' && !op.destIsDir) return op.dest;
  if (op.kind === 'TRANSFORM') return written(op.dest);
  return undefined;
}

/**
 * Lint a validated program and its plan. `source` is the Sitefile text (for comments),
 * `disabled` the rule ids turned off by the caller (`--lint-disable`).
 */
export function lint(program: Program, plan: Plan, source: string, disabled: readonly string[] = []): string[] {
  const file = program.file;
  const comments: CommentLine[] = [];
  const lines = logicalLines(source, file, comments);
  const byStart = new Map(lines.map((l) => [`${l.positions[0]?.line}:${l.positions[0]?.column}`, l]));
  const found: Found[] = [];
  const { file: offInFile, lines: offPerLine } = directives(comments, lines, found);
  const off = new Set([...offInFile, ...disabled]);
  const report = (line: number, rule: LintRule, message: string): void => {
    found.push({ line, rule, message });
  };

  // ---- unused-name: a declaration nothing refers to.
  const used = new Set<string>();
  // A template file (or one from an import or stage) can read any ARG, LET or value import.
  let opaqueTemplates = false;
  for (const ins of program.instructions) {
    for (const op of operands(ins)) for (const p of op.parts) if (p.kind === 'var') used.add(p.name);
    if (ins.kind === 'RENDER') {
      if (isInline(ins.template)) {
        try {
          const body: InlineBody = ins.template;
          templateNames(parseRenderTemplate(body.text, file, body.bodyLine)).forEach((n) => used.add(n));
        } catch {
          opaqueTemplates = true; // a template error is reported by the build; assume it may use anything
        }
      } else {
        opaqueTemplates = true;
      }
    }
    if ((ins.kind === 'RENDER' || ins.kind === 'TRANSFORM') && ins.each) used.add(ins.each.list);
    if (ins.kind === 'FEED') used.add(ins.list);
    if (ins.kind === 'TRANSFORM') used.add(ins.plugin);
  }
  for (const stage of plan.stages) {
    for (const op of stage.operations) {
      const src = op.kind === 'COPY' ? op.source : op.kind === 'RENDER' ? op.template : op.kind === 'TRANSFORM' ? op.source : undefined;
      if (src?.kind === 'import') used.add(src.name);
    }
  }
  for (const ins of program.instructions) {
    let what: string | undefined;
    let name = '';
    if (ins.kind === 'ARG' || ins.kind === 'LET') {
      if (!opaqueTemplates) [what, name] = [ins.kind, ins.name];
    } else if (ins.kind === 'IMPORT') {
      if (!opaqueTemplates || !VALUE_KINDS.has(ins.as ?? '')) [what, name] = ['IMPORT', ins.name];
    } else if (ins.kind === 'PLUGIN' || ins.kind === 'TOOL') {
      [what, name] = [ins.kind, ins.name];
    }
    if (what && !used.has(name)) report(ins.span.line, 'unused-name', `${what} ${name} is never used`);
  }

  // ---- unpinned-import: a remote IMPORT without INTEGRITY.
  for (const spec of plan.imports) {
    if (spec.location.kind === 'remote' && !spec.integrity) {
      report(spec.span.line, 'unpinned-import', `IMPORT ${spec.name} has no INTEGRITY, so ${spec.location.url} is trusted on first use (pin it with INTEGRITY sha256:…)`);
    }
  }

  // ---- unpinned-image: a registry image (FROM image, an oci EXPORT BASE) without a digest.
  const pinned = (ref: string): boolean | undefined => {
    try {
      return parseRef(ref).digest !== undefined;
    } catch {
      return undefined; // not a reference: the build reports it
    }
  };
  const unpinnedImage = (what: string, ref: string): string => `${what} ${ref} has no @sha256: digest, so the tag is trusted on first use (pin it as ${ref}@sha256:…)`;
  for (const stage of plan.stages) {
    if (stage.base.kind === 'image' && !stage.base.local && pinned(stage.base.ref) === false) {
      report(stage.span.line, 'unpinned-image', unpinnedImage(`${stage.base.lead} image`, stage.base.ref));
    }
  }
  const exp = program.instructions.find((i) => i.kind === 'EXPORT');
  if (exp && plan.exportFormat === 'oci' && plan.exportBase !== undefined && pinned(plan.exportBase) === false) {
    report(exp.span.line, 'unpinned-image', unpinnedImage('EXPORT … AS oci BASE', plan.exportBase));
  }

  for (const stage of plan.stages) {
    const ops = stage.operations;
    // ---- render-escape: an HTML page rendered without escaping.
    for (const op of ops) {
      if (op.kind !== 'RENDER' || op.escape !== 'none') continue;
      const dest = writtenPath(op)!;
      if (!/\.html?$/i.test(dest) || explicitEscape(byStart.get(`${op.span.line}:${op.span.column}`))) continue;
      report(op.span.line, 'render-escape', `RENDER writes ${dest} without --escape=html, so substituted values are not HTML-escaped (add --escape=html, or --escape=none if that is intended)`);
    }
    // ---- sitemap-order: a page written after the SITEMAP that should list it.
    ops.forEach((op, i) => {
      if (op.kind !== 'SITEMAP') return;
      const later = ops.slice(i + 1).find((o) => writtenPath(o)?.endsWith('.html'));
      if (later) {
        report(op.span.line, 'sitemap-order', `SITEMAP lists the .html files written before it, but line ${later.span.line} writes ${writtenPath(later)} after it (move SITEMAP after the pages)`);
      }
    });
    // ---- shadowed-write: a file replaced by a later --overwrite before anything reads it.
    const last = new Map<string, number>();
    for (const op of ops) {
      const path = op.kind === 'RENDER' && !op.each ? op.dest : op.kind === 'COPY' && op.source.kind === 'inline' ? op.dest : undefined;
      if (path === undefined) continue;
      const before = last.get(path);
      if (before !== undefined && op.overwrite) {
        report(before, 'shadowed-write', `${path} written here is replaced by line ${op.span.line} (--overwrite), so this write has no effect`);
      }
      last.set(path, op.span.line);
    }
  }

  const kept = found.filter((f) => f.rule === 'lint-comment' || !(off.has('all') || off.has(f.rule) || offPerLine.get(f.line)?.has('all') || offPerLine.get(f.line)?.has(f.rule)));
  // Stable: by line, then in the order the rules ran.
  const sorted = kept.map((f, i) => ({ f, i })).sort((a, b) => a.f.line - b.f.line || a.i - b.i);
  return sorted.map(({ f }) => `${file}:${f.line}: [${f.rule}] ${f.message}`);
}

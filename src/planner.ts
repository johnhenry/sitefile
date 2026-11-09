import { SitefileError, type SourceSpan } from './errors.js';
import { importedStage, isFragment, isImageBase, isInline, stageNames, type ImageBase, type EscapeMode, type RedirectStatus, type ExportFormat, type InlineBody, type ImportKind, type OciCompression, type Operand, type Program, type StageRef, type ValueType } from './language/ast.js';
import { normalizeLocal, normalizeVirtual } from './paths.js';
import { parseRef } from './ref.js';
import { checkPattern } from './fingerprint.js';
import { checkBaseUrl, checkHeaderName, checkHeaderValue, checkRedirectTarget, checkSitePath } from './site.js';
import { coerce, evaluateOperand, type Scalar } from './values.js';
import { isGitUri, parseGitUri, type GitLocation } from './resources/git.js';
import { parseFragmentBody } from './language/validate.js';
import type { TemplateNode } from './render.js';

/** An html LET: its template, parsed, evaluated where a RENDER uses it. */
export interface FragmentSpec {
  readonly name: string;
  /** The template as written (memo keys). */
  readonly text: string;
  readonly nodes: readonly TemplateNode[];
  readonly span: SourceSpan;
}

/** Where a COPY/RENDER reads from. */
export type SourceRef =
  /** Content written in the Sitefile itself (heredoc); `bodyLine` is its first Sitefile line. */
  | { readonly kind: 'inline'; readonly text: string; readonly delimiter: string; readonly bodyLine: number; readonly span: SourceSpan }
  /** `path` is a normalized virtual path inside an earlier stage's final tree (`--from=name`). */
  | { readonly kind: 'stage'; readonly name: string; readonly path: string; readonly span: SourceSpan }
  | { readonly kind: 'local'; readonly path: string; readonly span: SourceSpan }
  /** `path` is a normalized virtual path inside the import, `/` for the whole import. */
  | { readonly kind: 'import'; readonly name: string; readonly path: string; readonly span: SourceSpan };

export interface ImportSpec {
  readonly name: string;
  /** Interpolated URI as written. */
  readonly uri: string;
  readonly location:
    | { readonly kind: 'local'; readonly path: string }
    | { readonly kind: 'remote'; readonly url: string }
    | ({ readonly kind: 'git' } & GitLocation);
  readonly as?: ImportKind;
  readonly integrity?: string;
  readonly sort?: { readonly key: string; readonly descending: boolean };
  /** `SCHEMA <path>`: `uri` as written (for messages), `path` context-relative. */
  readonly schema?: { readonly uri: string; readonly path: string; readonly span: SourceSpan };
  /** `DRAFTS true|false`: false leaves out the elements whose `draft` is true. */
  readonly drafts?: boolean;
  /** Build secrets sent as request headers (ids and header names only; values come from --secret). */
  readonly secrets?: readonly { readonly id: string; readonly header: string; readonly bearer: boolean }[];
  /** `AS sitefile`: the `ARG NAME=value` clauses, evaluated (build args of the imported Sitefile). */
  readonly args?: readonly { readonly name: string; readonly value: string; readonly span: SourceSpan }[];
  readonly span: SourceSpan;
}

/** A declared renderer plugin: a local `.mjs` file in its own folder, or a `.wasm` module. */
export interface PluginSpec {
  readonly name: string;
  /** Context-relative path of the module. */
  readonly path: string;
  readonly integrity?: string;
  readonly span: SourceSpan;
}

/** A declared external program; it runs only when the CLI allows it (`--allow-exec`). */
export interface ToolSpec {
  readonly name: string;
  readonly span: SourceSpan;
}

/** A REDIRECT, evaluated; written to `/_redirects` at EXPORT. */
export interface RedirectSpec {
  readonly from: string;
  readonly to: string;
  readonly status: RedirectStatus;
  readonly overwrite: boolean;
  readonly span: SourceSpan;
}

/** A HEADER, evaluated; written to `/_headers` at EXPORT. */
export interface HeaderSpec {
  readonly pattern: string;
  readonly name: string;
  readonly value: string;
  readonly overwrite: boolean;
  readonly span: SourceSpan;
}

export type Operation =
  | {
      /** Report the references in the tree's HTML and CSS files that point at nothing. */
      readonly kind: 'CHECKLINKS';
      readonly warn: boolean;
      readonly external: boolean;
      /** The EXPORT path: absolute references resolve below it. */
      readonly root: string;
      /** Every REDIRECT source in the Sitefile: links to them are not broken. */
      readonly redirects: readonly string[];
      readonly span: SourceSpan;
    }
  | {
      /** Rename the files matching `patterns` to `name.<hash>.ext` and rewrite the HTML and CSS that reference them. */
      readonly kind: 'FINGERPRINT';
      /** Checked absolute patterns, NFC; a trailing `/` became `/**`. */
      readonly patterns: readonly string[];
      /** `MANIFEST <dest>`: the JSON map of old to new paths. */
      readonly manifest?: string;
      /** `INTEGRITY`: add `integrity="sha384-…"` to the script and stylesheet tags that load a renamed file. */
      readonly integrity?: true;
      /** The EXPORT path: absolute references (`/img/a.png`) in HTML and CSS name files below it. */
      readonly root: string;
      readonly overwrite: boolean;
      readonly span: SourceSpan;
    }
  | {
      /** sitemaps.org XML of the `.html` files in the tree at this point. */
      readonly kind: 'SITEMAP';
      /** Checked base URL without trailing slashes. */
      readonly base: string;
      readonly dest: string;
      readonly overwrite: boolean;
      readonly span: SourceSpan;
    }
  | {
      /** An Atom feed of a list import; `link` is interpolated per element (`${item…}`). */
      readonly kind: 'FEED';
      readonly list: string;
      readonly base: string;
      readonly link: Operand;
      readonly dest: string;
      readonly title: string;
      readonly overwrite: boolean;
      readonly span: SourceSpan;
    }
  | {
      /** Run a plugin; `dest` and option values are interpolated per FOR element at execution. */
      readonly kind: 'TRANSFORM';
      readonly source?: SourceRef;
      readonly plugin: string;
      readonly dest: Operand;
      readonly options: ReadonlyArray<{ readonly key: string; readonly value: Operand }>;
      readonly each?: { readonly as: string; readonly list: string };
      /** argv template when `plugin` names a TOOL. */
      readonly args?: readonly Operand[];
      /** A directory source's files to keep / leave out (`.sitefileignore` patterns, relative to it). */
      readonly include?: readonly string[];
      readonly exclude?: readonly string[];
      readonly overwrite: boolean;
      readonly span: SourceSpan;
    }
  | {
      readonly kind: 'COPY';
      readonly source: SourceRef;
      readonly dest: string;
      readonly destIsDir: boolean;
      readonly overwrite: boolean;
      /** `--exclude` patterns (`.sitefileignore` syntax, relative to the source directory). */
      readonly exclude?: readonly string[];
      /** `--chmod`: 0o644 or 0o755 for every copied file. */
      readonly chmod?: number;
      /** `--parents`: the source's path is kept under `dest`. */
      readonly parents?: true;
      readonly span: SourceSpan;
    }
  | {
      readonly kind: 'RENDER';
      readonly template: SourceRef;
      /** Normalized destination; '' when `each` is set (computed per element). */
      readonly dest: string;
      /**
       * `FOR as IN list [BY key] [PAGE size [FIRST <dest>]]`: render once per element (or group, or page); `dest`
       * (and `first`, for the first page) is interpolated per element.
       */
      readonly each?: {
        readonly as: string;
        readonly list: string;
        readonly dest: Operand;
        readonly by?: string;
        readonly page?: { readonly size: number; readonly first?: Operand };
      };
      readonly escape: EscapeMode;
      readonly overwrite: boolean;
      /** `--inline-svg`: `${file … | svg}` may paste SVG markup. */
      readonly inlineSvg?: true;
      readonly span: SourceSpan;
    };

/** What a stage's tree starts as. */
export type StageBase =
  | { readonly kind: 'scratch' }
  /** `path` is context-relative (`''` for the context root). */
  | { readonly kind: 'local'; readonly path: string }
  /** A copy of an earlier stage's final tree. */
  | { readonly kind: 'stage'; readonly name: string }
  /** `FROM image <ref> [AT <dir>]`: the files of an OCI site image. */
  | ImageStageBase;

export interface ImageStageBase {
  readonly kind: 'image';
  /** The evaluated reference, as written (validated). */
  readonly ref: string;
  /** No registry host: an image in the local store, never fetched. */
  readonly local: boolean;
  /** `AT`: normalized absolute directory inside the image (default: the image's root label). */
  readonly at?: string;
  /** `FROM` or `STAGE <name> FROM`, for messages. */
  readonly lead: string;
  readonly span: SourceSpan;
}

/** One FROM and the operations that follow it. */
export interface Stage {
  readonly name?: string;
  readonly base: StageBase;
  readonly operations: Operation[];
  readonly span: SourceSpan;
}

/** A fully evaluated build: no variables left, no IO done yet. */
export interface Plan {
  readonly file: string;
  /** In source order; EXPORT reads the last one. */
  readonly stages: readonly Stage[];
  readonly vars: ReadonlyMap<string, Scalar>;
  /** html LETs by name, in source order (not in `vars`: they are not evaluated here). */
  readonly fragments: ReadonlyMap<string, FragmentSpec>;
  readonly imports: readonly ImportSpec[];
  readonly plugins: readonly PluginSpec[];
  readonly tools: readonly ToolSpec[];
  readonly exportPath: string;
  readonly exportFormat: ExportFormat;
  /** Evaluated `BASE` clause of EXPORT, if any (raw string; validated when the export is written). */
  readonly exportBase: string | undefined;
  /** Evaluated `AT` clause of EXPORT ... AS oci: the root directory inside the image layer. */
  readonly exportRoot: string | undefined;
  /** `COMPRESSION` clause of EXPORT ... AS oci or wbn (undefined: gzip for oci, none for wbn). */
  readonly exportCompression: OciCompression | undefined;
  /** REDIRECT lines in source order (any stage); EXPORT writes them to `/_redirects`. */
  readonly redirects: readonly RedirectSpec[];
  /** HEADER lines in source order (any stage); EXPORT writes them to `/_headers`. */
  readonly headers: readonly HeaderSpec[];
  /**
   * Set when EXPORT adds `Cache-Control` lines for the fingerprinted files (and the pages) to `/_headers`:
   * the Sitefile has a FINGERPRINT, none of them says `--no-cache-headers`, and no HEADER sets
   * `Cache-Control` itself. `span` is the first FINGERPRINT's (the origin of the lines).
   */
  readonly cacheHeaders?: { readonly span: SourceSpan };
  readonly warnings: readonly string[];
  /**
   * `--target <stage>`: the build stops after this stage and exports its whole tree (see {@link targetPlan}).
   * Undefined for a normal build.
   */
  readonly target?: string;
}

const URL_RE = /^([A-Za-z][A-Za-z0-9+.-]*):\/\//;
const IMPORT_REF_RE = /^([A-Za-z_][A-Za-z0-9_]*):(.*)$/s;

/**
 * Evaluate a program. `argSource` names where a build arg came from in messages (`--build-arg NAME` for the
 * CLI's; an imported Sitefile's come from the `ARG` clauses of its IMPORT).
 */
export function plan(program: Program, buildArgs: Readonly<Record<string, string>> = {}, argSource: (name: string) => string = (n) => `--build-arg ${n}`): Plan {
  const vars = new Map<string, Scalar>();
  const fragments = new Map<string, FragmentSpec>();
  const imports = new Map<string, ImportSpec>();
  const stages: Stage[] = [];
  const plugins: PluginSpec[] = [];
  const tools: ToolSpec[] = [];
  const redirects: RedirectSpec[] = [];
  const headers: HeaderSpec[] = [];
  const fingerprints: Array<{ span: SourceSpan; headers: boolean }> = [];
  const warnings: string[] = [];
  const declaredArgs = new Set<string>();
  const allStageNames = stageNames(program.instructions);
  // validate() guarantees a FROM precedes every operation.
  const operations = (): Operation[] => stages[stages.length - 1]!.operations;
  let exportPath: string | undefined;
  let exportFormat: ExportFormat = 'directory';
  let exportBase: string | undefined;
  let exportRoot: string | undefined;
  let exportCompression: OciCompression | undefined;

  const bind = (name: string, raw: string, type: ValueType, span: SourceSpan, what: string): void => {
    vars.set(name, coerce(raw, type, span, what));
  };

  for (const ins of program.instructions) {
    switch (ins.kind) {
      case 'FROM':
        stages.push({
          ...(ins.alias !== undefined && { name: ins.alias }),
          base:
            ins.base === 'scratch'
              ? { kind: 'scratch' }
              : isImageBase(ins.base)
                ? imageBase(ins.base, vars, ins.keyword === 'STAGE' ? `STAGE ${ins.alias} FROM` : 'FROM')
                : stageBase(ins.base, vars, stages, allStageNames, imports, ins.keyword === 'STAGE' ? `STAGE ${ins.alias} FROM` : 'FROM'),
          operations: [],
          span: ins.span,
        });
        break;
      case 'ARG': {
        declaredArgs.add(ins.name);
        const given = buildArgs[ins.name];
        if (given !== undefined) bind(ins.name, given, ins.type, ins.span, argSource(ins.name));
        else if (ins.defaultValue) bind(ins.name, evaluateOperand(ins.defaultValue, vars), ins.type, ins.defaultValue.span, `ARG ${ins.name}`);
        else throw new SitefileError('E_MISSING_ARG', `ARG ${ins.name} has no default; pass --build-arg ${ins.name}=<value>`, ins.span);
        break;
      }
      case 'LET':
        if (isFragment(ins)) {
          fragments.set(ins.name, { name: ins.name, text: ins.body.text, nodes: parseFragmentBody(ins.body, program.file), span: ins.span });
          break;
        }
        bind(ins.name, evaluateOperand(ins.value, vars), ins.type, ins.value.span, `LET ${ins.name}`);
        break;
      case 'IMPORT': {
        const uri = evaluateOperand(ins.uri, vars);
        const spec: ImportSpec = {
          name: ins.name,
          uri,
          location: classifyUri(uri, ins.uri.span),
          ...(ins.as ? { as: ins.as } : {}),
          ...(ins.integrity ? { integrity: ins.integrity } : {}),
          ...(ins.sort ? { sort: { key: ins.sort.key, descending: ins.sort.descending } } : {}),
          ...(ins.schema ? { schema: schemaSpec(ins.name, ins.schema.path, vars, ins.schema.span) } : {}),
          ...(ins.drafts ? { drafts: draftsValue(ins.name, ins.drafts.value, vars) } : {}),
          ...(ins.secrets ? { secrets: ins.secrets.map((s) => ({ id: s.id, header: s.header, bearer: s.bearer })) } : {}),
          ...(ins.args ? { args: ins.args.map((a) => ({ name: a.name, value: evaluateOperand(a.value, vars), span: a.span })) } : {}),
          span: ins.span,
        };
        if (spec.as === 'sitefile' && spec.location.kind === 'remote') {
          throw new SitefileError('E_PROTOCOL', `IMPORT ${ins.name}: AS sitefile reads a context directory or a git+https:// repository, not a URL`, ins.uri.span);
        }
        if (spec.location.kind === 'local' && ins.secrets) {
          throw new SitefileError('E_VALIDATION', `IMPORT ${ins.name}: SECRET applies to remote imports (https:// or git+https://), not local paths`, ins.secrets[0]!.span);
        }
        if (spec.location.kind === 'git' && spec.integrity) {
          throw new SitefileError('E_VALIDATION', `IMPORT ${ins.name}: INTEGRITY does not apply to Git imports; pin a commit instead (#<40 hex>[:path])`, ins.span);
        }
        if (spec.location.kind === 'local' && spec.integrity) {
          warnings.push(`${program.file}:${ins.span.line}: INTEGRITY on a local import is checked but not recorded in the lockfile`);
        }
        imports.set(ins.name, spec);
        break;
      }
      case 'COPY': {
        const dest = normalizeVirtual(evaluateOperand(ins.dest, vars), ins.dest.span, 'COPY destination');
        if (isInline(ins.source) && dest.trailingSlash) {
          throw new SitefileError('E_PATH', `COPY of an inline body needs a file destination, not a directory`, ins.dest.span);
        }
        operations().push({
          kind: 'COPY',
          source: sourceRef(ins.source, vars, imports, ins.from),
          dest: dest.path,
          destIsDir: dest.trailingSlash,
          overwrite: ins.overwrite,
          ...(ins.exclude ? { exclude: ins.exclude } : {}),
          ...(ins.chmod !== undefined ? { chmod: ins.chmod } : {}),
          ...(ins.parents ? { parents: true as const } : {}),
          span: ins.span,
        });
        break;
      }
      case 'RENDER': {
        if (ins.each) {
          operations().push({
            kind: 'RENDER',
            template: sourceRef(ins.template, vars, imports, ins.from),
            dest: '',
            each: {
              as: ins.each.as,
              list: ins.each.list,
              dest: ins.dest,
              ...(ins.each.by ? { by: ins.each.by.key } : {}),
              ...(ins.each.page
                ? { page: { size: pageSize(ins.each.page.size, vars), ...(ins.each.page.first ? { first: ins.each.page.first } : {}) } }
                : {}),
            },
            escape: ins.escape,
            overwrite: ins.overwrite,
            ...(ins.inlineSvg ? { inlineSvg: true as const } : {}),
            span: ins.span,
          });
          break;
        }
        const raw = evaluateOperand(ins.dest, vars);
        const dest = normalizeVirtual(raw, ins.dest.span, 'RENDER destination');
        if (dest.trailingSlash) {
          throw new SitefileError('E_PATH', `RENDER destination '${raw}' must be a file path, not a directory`, ins.dest.span);
        }
        operations().push({
          kind: 'RENDER',
          template: sourceRef(ins.template, vars, imports, ins.from),
          dest: dest.path,
          escape: ins.escape,
          overwrite: ins.overwrite,
          ...(ins.inlineSvg ? { inlineSvg: true as const } : {}),
          span: ins.span,
        });
        break;
      }
      case 'PLUGIN': {
        const raw = evaluateOperand(ins.uri, vars);
        if (URL_RE.test(raw)) {
          throw new SitefileError('E_PROTOCOL', `PLUGIN ${ins.name}: plugins must be local files in the build context, not URLs`, ins.uri.span);
        }
        const rel = normalizeLocal(raw, ins.uri.span).path;
        if (!/\.(mjs|wasm)$/.test(rel)) {
          throw new SitefileError('E_VALIDATION', `PLUGIN ${ins.name}: '${raw}' must be an ES module (.mjs) or a WebAssembly module (.wasm)`, ins.uri.span);
        }
        plugins.push({ name: ins.name, path: rel, ...(ins.integrity ? { integrity: ins.integrity } : {}), span: ins.span });
        break;
      }
      case 'TOOL':
        tools.push({ name: ins.name, span: ins.span });
        break;
      case 'TRANSFORM':
        operations().push({
          kind: 'TRANSFORM',
          ...(ins.source ? { source: sourceRef(ins.source, vars, imports, ins.from) } : {}),
          plugin: ins.plugin,
          dest: ins.dest,
          options: ins.options.map((o) => ({ key: o.key, value: o.value })),
          ...(ins.each ? { each: { as: ins.each.as, list: ins.each.list } } : {}),
          ...(ins.args ? { args: ins.args } : {}),
          ...(ins.include ? { include: ins.include } : {}),
          ...(ins.exclude ? { exclude: ins.exclude } : {}),
          overwrite: ins.overwrite,
          span: ins.span,
        });
        break;
      case 'SITEMAP': {
        const base = checkBaseUrl(evaluateOperand(ins.base, vars), 'SITEMAP BASE', ins.base.span);
        operations().push({ kind: 'SITEMAP', base, dest: fileDest(ins.dest, vars, '/sitemap.xml', 'SITEMAP'), overwrite: ins.overwrite, span: ins.span });
        break;
      }
      case 'FINGERPRINT': {
        const patterns = ins.patterns.map((o) => checkPattern(evaluateOperand(o, vars), o.span));
        const manifest = ins.manifest ? fileDest(ins.manifest, vars, '', 'FINGERPRINT MANIFEST') : undefined;
        // `root` is the EXPORT path, filled in below (EXPORT is the last instruction).
        operations().push({
          kind: 'FINGERPRINT',
          patterns,
          ...(manifest !== undefined ? { manifest } : {}),
          ...(ins.integrity && { integrity: true as const }),
          root: '/',
          overwrite: ins.overwrite,
          span: ins.span,
        });
        fingerprints.push({ span: ins.span, headers: !ins.noCacheHeaders });
        break;
      }
      case 'CHECKLINKS':
        // `root` and `redirects` are filled in below, once every line is planned.
        operations().push({ kind: 'CHECKLINKS', warn: ins.warn, external: ins.external, root: '/', redirects: [], span: ins.span });
        break;
      case 'FEED': {
        const base = checkBaseUrl(evaluateOperand(ins.base, vars), 'FEED BASE', ins.base.span);
        operations().push({
          kind: 'FEED',
          list: ins.list,
          base,
          link: ins.link,
          dest: fileDest(ins.dest, vars, '/feed.xml', 'FEED'),
          title: ins.title ? evaluateOperand(ins.title, vars) : base,
          overwrite: ins.overwrite,
          span: ins.span,
        });
        break;
      }
      case 'REDIRECT':
        redirects.push({
          from: checkSitePath(evaluateOperand(ins.from, vars), 'REDIRECT source', ins.from.span),
          to: checkRedirectTarget(evaluateOperand(ins.to, vars), ins.to.span),
          status: ins.status,
          overwrite: ins.overwrite,
          span: ins.span,
        });
        break;
      case 'HEADER': {
        const name = checkHeaderName(evaluateOperand(ins.name, vars), ins.name.span);
        headers.push({
          pattern: checkSitePath(evaluateOperand(ins.pattern, vars), 'HEADER path pattern', ins.pattern.span),
          name,
          value: checkHeaderValue(ins.value.map((v) => evaluateOperand(v, vars)).join(' '), name, ins.value[0]!.span),
          overwrite: ins.overwrite,
          span: ins.span,
        });
        break;
      }
      case 'EXPORT':
        exportPath = normalizeVirtual(evaluateOperand(ins.path, vars), ins.path.span, 'EXPORT path').path;
        exportFormat = ins.format;
        exportBase = ins.base ? evaluateOperand(ins.base, vars) : undefined;
        if (ins.at && ins.format !== 'oci') throw new SitefileError('E_VALIDATION', 'EXPORT ... AT applies only to AS oci', ins.at.span);
        exportRoot = ins.at ? normalizeVirtual(evaluateOperand(ins.at, vars), ins.at.span, 'EXPORT AT path').path : undefined;
        if (ins.compression && ins.format !== 'oci' && ins.format !== 'wbn' && ins.format !== 'warc') throw new SitefileError('E_VALIDATION', 'EXPORT ... COMPRESSION applies only to AS oci, AS wbn and AS warc', ins.compression.span);
        exportCompression = ins.compression?.value;
        break;
    }
  }

  for (const st of stages) {
    st.operations.forEach((op, i) => {
      if (op.kind === 'FINGERPRINT') st.operations[i] = { ...op, root: exportPath ?? '/' };
      if (op.kind === 'CHECKLINKS') st.operations[i] = { ...op, root: exportPath ?? '/', redirects: redirects.map((r) => r.from) };
    });
  }
  // An unnamed stage can be neither a FROM base nor a COPY --from source; only the last one is useful (EXPORT reads it).
  for (const st of stages.slice(0, -1)) {
    if (st.name === undefined) {
      warnings.push(`${program.file}:${st.span.line}: unnamed stage is not the last one, so its output can never be used; name it with FROM … AS <name>`);
    }
  }
  for (const name of Object.keys(buildArgs)) {
    if (!declaredArgs.has(name)) warnings.push(`--build-arg ${name} does not match any ARG and was ignored`);
  }
  if (stages.length === 0 || exportPath === undefined) {
    // validate() guarantees both; this keeps the types honest.
    throw new SitefileError('E_VALIDATION', 'a Sitefile needs FROM and EXPORT');
  }
  // FINGERPRINT's Cache-Control lines: unless turned off, or a HEADER already sets Cache-Control.
  const firstFingerprint = fingerprints[0];
  const cacheHeaders =
    firstFingerprint && fingerprints.every((f) => f.headers) && !headers.some((h) => h.name.toLowerCase() === 'cache-control')
      ? { span: firstFingerprint.span }
      : undefined;
  return {
    file: program.file,
    stages,
    vars,
    fragments,
    imports: [...imports.values()],
    plugins,
    tools,
    exportPath,
    exportFormat,
    exportBase,
    exportRoot,
    exportCompression,
    redirects,
    headers,
    ...(cacheHeaders ? { cacheHeaders } : {}),
    warnings,
  };
}

/**
 * `--target <name>`: keep the stages up to and including the named one, and export that stage's whole tree
 * (`/`) instead of the EXPORT path. The EXPORT's format and its options (AS, BASE, AT, COMPRESSION) still apply;
 * the `/_redirects` and `/_headers` files EXPORT writes are not (they belong to the EXPORT path). An unknown
 * name is a usage error that lists the named stages.
 */
export function targetPlan(p: Plan, name: string): Plan {
  const index = p.stages.findIndex((s) => s.name === name);
  if (index === -1) {
    const names = p.stages.flatMap((s) => (s.name === undefined ? [] : [s.name]));
    throw new SitefileError(
      'E_USAGE',
      names.length
        ? `--target '${name}' does not name a stage (stages: ${names.join(', ')})`
        : `--target '${name}' does not name a stage (the Sitefile has no named stages; name one with FROM … AS <name>)`,
    );
  }
  const { cacheHeaders: _, ...rest } = p;
  return { ...rest, stages: p.stages.slice(0, index + 1), exportPath: '/', redirects: [], headers: [], target: name };
}

/** `IMPORT … SCHEMA <path>`: a local `.json`, `.yaml` or `.yml` file in the build context. */
function schemaSpec(name: string, op: Operand, vars: ReadonlyMap<string, Scalar>, span: SourceSpan): { uri: string; path: string; span: SourceSpan } {
  const uri = evaluateOperand(op, vars);
  if (URL_RE.test(uri)) {
    throw new SitefileError('E_PROTOCOL', `IMPORT ${name}: SCHEMA must be a local file in the build context, not a URL`, op.span);
  }
  const rel = normalizeLocal(uri, op.span).path;
  if (!/\.(json|ya?ml)$/i.test(rel)) {
    throw new SitefileError('E_VALIDATION', `IMPORT ${name}: SCHEMA '${uri}' must be a .json, .yaml or .yml file`, op.span);
  }
  return { uri, path: rel, span };
}

/** `IMPORT … DRAFTS <value>`: `true` keeps drafts, `false` leaves them out. */
function draftsValue(name: string, op: Operand, vars: ReadonlyMap<string, Scalar>): boolean {
  const raw = evaluateOperand(op, vars);
  if (raw === 'true' || raw === 'false') return raw === 'true';
  throw new SitefileError('E_VALIDATION', `IMPORT ${name}: DRAFTS must be true or false, got '${raw}'`, op.span);
}

/** `RENDER … PAGE <n>`: a whole number of items per page, at least 1. */
function pageSize(op: Operand, vars: ReadonlyMap<string, Scalar>): number {
  const raw = evaluateOperand(op, vars);
  if (!/^[1-9][0-9]{0,8}$/.test(raw)) {
    throw new SitefileError('E_VALIDATION', `RENDER … PAGE: '${raw}' is not a page size (a whole number from 1 to 999999999)`, op.span);
  }
  return Number(raw);
}

/** `TO <dest>` of SITEMAP/FEED or `MANIFEST <dest>` of FINGERPRINT (a file path), or the default. */
function fileDest(op: Operand | undefined, vars: ReadonlyMap<string, Scalar>, fallback: string, what: string): string {
  if (!op) return fallback;
  const raw = evaluateOperand(op, vars);
  const dest = normalizeVirtual(raw, op.span, `${what} destination`);
  if (dest.trailingSlash) throw new SitefileError('E_PATH', `${what} destination '${raw}' must be a file path, not a directory`, op.span);
  return dest.path;
}

function classifyUri(uri: string, span: SourceSpan): ImportSpec['location'] {
  if (isGitUri(uri)) return { kind: 'git', ...parseGitUri(uri, span) };
  const m = URL_RE.exec(uri);
  if (m) {
    const scheme = m[1]!.toLowerCase();
    if (scheme === 'https' || scheme === 'http') return { kind: 'remote', url: uri };
    if (scheme === 'file') {
      throw new SitefileError('E_PROTOCOL', `file: URLs are not allowed; use a path relative to the build context`, span);
    }
    throw new SitefileError('E_PROTOCOL', `unsupported protocol '${scheme}:' (supported: https, git+https, relative paths)`, span);
  }
  return { kind: 'local', path: normalizeLocal(uri, span).path };
}

/** `image <ref> [AT <dir>]`: the reference must parse; a reference without a registry host names a local-store image. */
function imageBase(b: ImageBase, vars: ReadonlyMap<string, Scalar>, lead: string): ImageStageBase {
  const ref = evaluateOperand(b.ref, vars);
  let local: boolean;
  try {
    local = parseRef(ref).registry === undefined;
  } catch (e) {
    if (e instanceof SitefileError) throw new SitefileError('E_VALIDATION', `${lead} image: ${e.detail}`, b.ref.span, { cause: e });
    throw e;
  }
  const at = b.at ? normalizeVirtual(evaluateOperand(b.at, vars), b.at.span, 'image directory').path : undefined;
  return { kind: 'image', ref, local, ...(at !== undefined ? { at } : {}), lead, span: b.ref.span };
}

/** A bare identifier naming an earlier stage is a stage; anything else is a context-relative directory. */
function stageBase(
  op: Operand,
  vars: ReadonlyMap<string, Scalar>,
  earlier: readonly Stage[],
  all: ReadonlySet<string>,
  imports: ReadonlyMap<string, ImportSpec>,
  lead = 'FROM',
): StageBase {
  const raw = evaluateOperand(op, vars);
  if (earlier.some((s) => s.name === raw)) return { kind: 'stage', name: raw };
  // `<import>+<stage>`: a stage of a Sitefile imported AS sitefile on an earlier line.
  const imported = importedStage(raw);
  if (imported && imports.get(imported.import)?.as === 'sitefile') return { kind: 'stage', name: raw };
  if (all.has(raw)) {
    throw new SitefileError('E_UNDEFINED', `${lead} ${raw}: stage '${raw}' is not defined before this line (write ./${raw} for a local directory)`, op.span);
  }
  return { kind: 'local', path: normalizeLocal(raw, op.span).path };
}

function sourceRef(
  op: Operand | InlineBody,
  vars: ReadonlyMap<string, Scalar>,
  imports: ReadonlyMap<string, ImportSpec>,
  from?: StageRef,
): SourceRef {
  if (isInline(op)) return { kind: 'inline', text: op.text, delimiter: op.delimiter, bodyLine: op.bodyLine, span: op.span };
  const raw = evaluateOperand(op, vars);
  if (from) return { kind: 'stage', name: from.name, path: normalizeVirtual(raw, op.span, `path inside stage '${from.name}'`).path, span: op.span };
  if (URL_RE.test(raw)) {
    throw new SitefileError('E_VALIDATION', `cannot read '${raw}' directly; IMPORT it under a name, then COPY name:`, op.span);
  }
  const m = IMPORT_REF_RE.exec(raw);
  if (m) {
    const name = m[1]!;
    const inner = m[2]!;
    if (!imports.has(name)) {
      throw new SitefileError('E_UNDEFINED', `'${name}:' does not name an IMPORT; write './${raw}' for a local file`, op.span);
    }
    const path = inner === '' ? '/' : normalizeVirtual(inner, op.span, `path inside import '${name}'`).path;
    return { kind: 'import', name, path, span: op.span };
  }
  return { kind: 'local', path: normalizeLocal(raw, op.span).path, span: op.span };
}

/** A reference to a stage of an imported Sitefile (`FROM theme+assets`, `COPY --from=theme+assets …`). */
export interface ImportedStageRef {
  /** `theme+assets` */
  readonly name: string;
  readonly import: string;
  readonly stage: string;
  /** The instruction that names it. */
  readonly span: SourceSpan;
}

/** Every reference to a stage of an imported Sitefile, in source order (stage bases, then `--from` sources, per stage). */
export function importedStageRefs(stages: readonly Stage[]): ImportedStageRef[] {
  const out: ImportedStageRef[] = [];
  const add = (name: string, span: SourceSpan): void => {
    const s = importedStage(name);
    if (s) out.push({ name, import: s.import, stage: s.stage, span });
  };
  for (const st of stages) {
    if (st.base.kind === 'stage') add(st.base.name, st.span);
    for (const op of st.operations) {
      const src = op.kind === 'COPY' ? op.source : op.kind === 'RENDER' ? op.template : op.kind === 'TRANSFORM' ? op.source : undefined;
      if (src?.kind === 'stage') add(src.name, op.span);
    }
  }
  return out;
}

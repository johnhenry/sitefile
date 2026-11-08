//! A validated program to a fully evaluated build plan (src/planner.ts):
//! variables bound, paths normalized, no IO.

use crate::build_args::BuildArgs;
use crate::error::{ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::fingerprint::check_pattern;
use crate::language::ast::{
    imported_stage, stage_names, EscapeMode, ExportFormat, FromBase, ImportKind, Instruction,
    OciCompression, Operand, Program, Source, StageRef, TemplatePart, ValueType,
};
use crate::paths::{normalize_local, normalize_virtual};
use crate::site::{
    check_base_url, check_header_name, check_header_value, check_redirect_target, check_site_path,
};
use crate::values::{coerce, evaluate_operand, Env, Scalar};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;

/// Where a COPY/RENDER/TRANSFORM reads from.
#[derive(Debug, Clone, PartialEq)]
pub enum SourceRef {
    /// Content written in the Sitefile itself (heredoc); `body_line` is its first Sitefile line.
    Inline {
        text: String,
        delimiter: String,
        body_line: u32,
        span: SourceSpan,
    },
    /// `path` is a normalized virtual path inside an earlier stage's final tree (`--from=name`).
    Stage {
        name: String,
        path: String,
        span: SourceSpan,
    },
    /// A context-relative path.
    Local { path: String, span: SourceSpan },
    /// `path` is a normalized virtual path inside the import, `/` for the whole import.
    Import {
        name: String,
        path: String,
        span: SourceSpan,
    },
}

impl SourceRef {
    pub fn span(&self) -> &SourceSpan {
        match self {
            SourceRef::Inline { span, .. }
            | SourceRef::Stage { span, .. }
            | SourceRef::Local { span, .. }
            | SourceRef::Import { span, .. } => span,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportLocation {
    Local {
        path: String,
    },
    Remote {
        url: String,
    },
    /// `git+https://…#ref:path` (src/resources/git.ts).
    Git(crate::resources::git::GitLocation),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSort {
    pub key: String,
    pub descending: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportSpec {
    pub name: String,
    /// Interpolated URI as written.
    pub uri: String,
    pub location: ImportLocation,
    pub as_kind: Option<ImportKind>,
    pub integrity: Option<String>,
    pub sort: Option<ImportSort>,
    /// `SCHEMA <path>`: every document must match the schema file.
    pub schema: Option<ImportSchema>,
    /// `DRAFTS true|false`: false leaves out the elements whose `draft` is true.
    pub drafts: Option<bool>,
    /// Build secrets sent as request headers (ids and header names only; values come from --secret).
    pub secrets: Vec<SecretRef>,
    /// `AS sitefile`: the `ARG NAME=value` clauses, evaluated (build args of the imported Sitefile).
    pub args: Vec<PlannedArg>,
    pub span: SourceSpan,
}

/// An `ARG NAME=value` clause of an `IMPORT … AS sitefile`, evaluated.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedArg {
    pub name: String,
    pub value: String,
    pub span: SourceSpan,
}

/// An `IMPORT … SCHEMA` clause as planned: `uri` as written (for messages), `path` context-relative.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSchema {
    pub uri: String,
    pub path: String,
    pub span: SourceSpan,
}

/// An `IMPORT … SECRET` clause as planned: which secret, which header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRef {
    pub id: String,
    pub header: String,
    pub bearer: bool,
}

/// A declared renderer plugin: a local `.mjs` file in its own folder, or a `.wasm` module.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginSpec {
    pub name: String,
    /// Context-relative path of the module.
    pub path: String,
    pub integrity: Option<String>,
    pub span: SourceSpan,
}

/// A declared external program; it runs only when the CLI allows it (`--allow-exec`).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub span: SourceSpan,
}

/// One `key=value` of a TRANSFORM's OPTIONS (the value is interpolated at execution).
#[derive(Debug, Clone, PartialEq)]
pub struct PlanOption {
    pub key: String,
    pub value: Operand,
}

/// `FOR as IN list` of a TRANSFORM.
#[derive(Debug, Clone, PartialEq)]
pub struct TransformEach {
    pub as_name: String,
    pub list: String,
}

/// `FOR as IN list [BY key] [PAGE size [FIRST <dest>]]` of a RENDER: `dest` (and `first`, for the first page)
/// is interpolated per element, group or page.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderEach {
    pub as_name: String,
    pub list: String,
    pub dest: Operand,
    pub by: Option<String>,
    pub page: Option<RenderPage>,
}

/// `PAGE size [FIRST <dest>]`, evaluated.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderPage {
    pub size: usize,
    pub first: Option<Operand>,
}

/// A REDIRECT, evaluated; written to `/_redirects` at EXPORT.
#[derive(Debug, Clone, PartialEq)]
pub struct RedirectSpec {
    pub from: String,
    pub to: String,
    pub status: u16,
    pub overwrite: bool,
    pub span: SourceSpan,
}

/// A HEADER, evaluated; written to `/_headers` at EXPORT.
#[derive(Debug, Clone, PartialEq)]
pub struct HeaderSpec {
    pub pattern: String,
    pub name: String,
    pub value: String,
    pub overwrite: bool,
    pub span: SourceSpan,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Operation {
    /// Rename the files matching `patterns` (checked, NFC; a trailing `/` became `/**`) to `name.<hash>.ext`
    /// and rewrite the HTML and CSS that reference them; `manifest` is where the JSON map of renames goes.
    /// `root` is the EXPORT path: absolute references (`/img/a.png`) in HTML and CSS name files below it.
    Fingerprint {
        patterns: Vec<String>,
        manifest: Option<String>,
        /// `INTEGRITY`: add `integrity="sha384-…"` to the script and stylesheet tags that load a renamed file.
        integrity: bool,
        root: String,
        overwrite: bool,
        span: SourceSpan,
    },
    /// Report the references in the tree's HTML and CSS files that point at nothing; `root` is the EXPORT path and
    /// `redirects` every REDIRECT source in the Sitefile.
    CheckLinks {
        warn: bool,
        external: bool,
        root: String,
        redirects: Vec<String>,
        span: SourceSpan,
    },
    /// sitemaps.org XML of the `.html` files in the tree at this point (`base` is checked, without trailing slashes).
    Sitemap {
        base: String,
        dest: String,
        overwrite: bool,
        span: SourceSpan,
    },
    /// An Atom feed of a list import; `link` is interpolated per element (`${item…}`).
    Feed {
        list: String,
        base: String,
        link: Operand,
        dest: String,
        title: String,
        overwrite: bool,
        span: SourceSpan,
    },
    /// Run a plugin or tool; `dest` and option values are interpolated per FOR element at execution.
    Transform {
        source: Option<SourceRef>,
        plugin: String,
        dest: Operand,
        options: Vec<PlanOption>,
        each: Option<TransformEach>,
        /// argv template when `plugin` names a TOOL.
        args: Option<Vec<Operand>>,
        /// A directory source's files to keep / leave out (`.sitefileignore` patterns, relative to it).
        include: Vec<String>,
        exclude: Vec<String>,
        overwrite: bool,
        span: SourceSpan,
    },
    /// `exclude`, `chmod`, `parents`: COPY's `--exclude`, `--chmod` and `--parents`.
    Copy {
        source: SourceRef,
        dest: String,
        dest_is_dir: bool,
        overwrite: bool,
        exclude: Vec<String>,
        chmod: Option<u32>,
        parents: bool,
        span: SourceSpan,
    },
    Render {
        template: SourceRef,
        /// Normalized destination; `""` when `each` is set (computed per element).
        dest: String,
        each: Option<RenderEach>,
        escape: EscapeMode,
        overwrite: bool,
        /// `--inline-svg`: `${file … | svg}` may paste SVG markup.
        inline_svg: bool,
        span: SourceSpan,
    },
}

impl Operation {
    pub fn span(&self) -> &SourceSpan {
        match self {
            Operation::Transform { span, .. }
            | Operation::Copy { span, .. }
            | Operation::Render { span, .. }
            | Operation::Sitemap { span, .. }
            | Operation::Fingerprint { span, .. }
            | Operation::CheckLinks { span, .. }
            | Operation::Feed { span, .. } => span,
        }
    }
    pub fn keyword(&self) -> &'static str {
        match self {
            Operation::Transform { .. } => "TRANSFORM",
            Operation::Copy { .. } => "COPY",
            Operation::Render { .. } => "RENDER",
            Operation::Sitemap { .. } => "SITEMAP",
            Operation::Fingerprint { .. } => "FINGERPRINT",
            Operation::CheckLinks { .. } => "CHECKLINKS",
            Operation::Feed { .. } => "FEED",
        }
    }
}

/// What a stage's tree starts as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageBase {
    Scratch,
    /// `path` is context-relative (`""` for the context root).
    Local {
        path: String,
    },
    /// A copy of an earlier stage's final tree.
    Stage {
        name: String,
    },
    /// `FROM image <ref> [AT <dir>]`: the files of an OCI site image. `reference` is the evaluated
    /// reference as written; `local` (no registry host) names an image in the local store, never
    /// fetched; `at` is the normalized directory inside the image; `lead` is `FROM` or
    /// `STAGE <name> FROM`, for messages.
    Image {
        reference: String,
        local: bool,
        at: Option<String>,
        lead: String,
        span: SourceSpan,
    },
}

/// One FROM and the operations that follow it.
#[derive(Debug, Clone, PartialEq)]
pub struct Stage {
    pub name: Option<String>,
    pub base: StageBase,
    pub operations: Vec<Operation>,
    pub span: SourceSpan,
}

/// A fully evaluated build: no variables left, no IO done yet.
/// An html LET: its template, parsed, evaluated where a RENDER uses it.
#[derive(Debug, Clone)]
pub struct FragmentSpec {
    pub name: String,
    /// The template as written (memo keys).
    pub text: String,
    pub nodes: std::sync::Arc<Vec<crate::render::TemplateNode>>,
    pub span: SourceSpan,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub file: String,
    /// In source order; EXPORT reads the last one.
    pub stages: Vec<Stage>,
    pub vars: Env,
    /// Names of `vars` in declaration order (JavaScript Map order).
    pub var_order: Vec<String>,
    /// html LETs, in source order (not in `vars`: they are not evaluated here).
    pub fragments: Vec<FragmentSpec>,
    pub imports: Vec<ImportSpec>,
    pub plugins: Vec<PluginSpec>,
    pub tools: Vec<ToolSpec>,
    pub export_path: String,
    pub export_format: ExportFormat,
    /// Evaluated `BASE` clause of EXPORT, if any (raw string; validated when the export is written).
    pub export_base: Option<String>,
    /// Evaluated `AT` clause of `EXPORT … AS oci`: the root directory inside the image layer.
    pub export_root: Option<String>,
    /// `COMPRESSION` clause of EXPORT ... AS oci or wbn (None: gzip for oci, none for wbn).
    pub export_compression: Option<OciCompression>,
    /// `KEY <secret-id>` of EXPORT ... AS swbn: the --secret holding the signing key.
    pub export_key: Option<String>,
    /// REDIRECT lines in source order (any stage); EXPORT writes them to `/_redirects`.
    pub redirects: Vec<RedirectSpec>,
    /// HEADER lines in source order (any stage); EXPORT writes them to `/_headers`.
    pub headers: Vec<HeaderSpec>,
    /// Set (to the first FINGERPRINT's span, the origin of the lines) when EXPORT adds `Cache-Control` lines
    /// for the fingerprinted files and the pages to `/_headers`: the Sitefile has a FINGERPRINT, none says
    /// `--no-cache-headers`, and no HEADER sets `Cache-Control` itself.
    pub cache_headers: Option<SourceSpan>,
    pub warnings: Vec<String>,
    /// `--target <stage>`: the build stops after this stage and exports its whole tree (see [`target_plan`]).
    pub target: Option<String>,
}

/// `--target <name>`: keep the stages up to and including the named one, and export that stage's whole tree
/// (`/`) instead of the EXPORT path. The EXPORT's format and its options (AS, BASE, AT, COMPRESSION) still apply;
/// the `/_redirects` and `/_headers` files EXPORT writes are not (they belong to the EXPORT path). An unknown
/// name is a usage error that lists the named stages.
pub fn target_plan(mut p: Plan, name: &str) -> Result<Plan> {
    let Some(index) = p
        .stages
        .iter()
        .position(|s| s.name.as_deref() == Some(name))
    else {
        let names: Vec<&str> = p.stages.iter().filter_map(|s| s.name.as_deref()).collect();
        let msg = if names.is_empty() {
            format!("--target '{name}' does not name a stage (the Sitefile has no named stages; name one with FROM … AS <name>)")
        } else {
            format!(
                "--target '{name}' does not name a stage (stages: {})",
                names.join(", ")
            )
        };
        return Err(SitefileError::plain(E_USAGE, msg));
    };
    p.stages.truncate(index + 1);
    p.export_path = "/".into();
    p.redirects.clear();
    p.headers.clear();
    p.cache_headers = None;
    p.target = Some(name.to_string());
    Ok(p)
}

/// `^([A-Za-z][A-Za-z0-9+.-]*):\/\/`: the scheme of a URL.
fn url_scheme(s: &str) -> Option<&str> {
    let (scheme, rest) = s.split_once(':')?;
    let mut chars = scheme.chars();
    let ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
        && rest.starts_with("//");
    ok.then_some(scheme)
}

/// `^([A-Za-z_][A-Za-z0-9_]*):(.*)$`: `name:inner`.
fn import_ref(s: &str) -> Option<(&str, &str)> {
    let (name, inner) = s.split_once(':')?;
    crate::language::ast::is_ident(name).then_some((name, inner))
}

/// Plan a validated program. `build_args` are `--build-arg` values (strings,
/// coerced to each ARG's type).
pub fn plan(program: &Program, build_args: &BuildArgs) -> Result<Plan> {
    plan_with(program, build_args, &|n| format!("--build-arg {n}"))
}

/// [`plan`], where `arg_source` names where a build arg came from in messages (an imported Sitefile's
/// come from the `ARG` clauses of its IMPORT).
pub fn plan_with(
    program: &Program,
    build_args: &BuildArgs,
    arg_source: &dyn Fn(&str) -> String,
) -> Result<Plan> {
    let mut vars = Env::new();
    let mut fragments: Vec<FragmentSpec> = Vec::new();
    let mut var_order: Vec<String> = Vec::new();
    let mut imports: Vec<ImportSpec> = Vec::new();
    let mut stages: Vec<Stage> = Vec::new();
    let mut plugins = Vec::new();
    let mut tools = Vec::new();
    let mut redirects = Vec::new();
    let mut headers = Vec::new();
    // Each FINGERPRINT's span, and whether it allows the Cache-Control lines.
    let mut fingerprints: Vec<(SourceSpan, bool)> = Vec::new();
    let mut warnings = Vec::new();
    let mut declared_args: BTreeSet<&str> = BTreeSet::new();
    let all_stage_names = stage_names(&program.instructions);
    let mut export_path: Option<String> = None;
    let mut export_format = ExportFormat::Directory;
    let mut export_base = None;
    let mut export_root = None;
    let mut export_compression = None;
    let mut export_key = None;

    fn bind(
        vars: &mut Env,
        order: &mut Vec<String>,
        name: &str,
        raw: &str,
        ty: ValueType,
        span: &SourceSpan,
        what: &str,
    ) -> Result<()> {
        let v = coerce(raw, ty, Some(span), what)?;
        if vars.insert(name.to_string(), v).is_none() {
            order.push(name.to_string());
        }
        Ok(())
    }

    // validate() guarantees a FROM precedes every operation.
    fn operations<'a>(
        stages: &'a mut [Stage],
        span: &SourceSpan,
    ) -> Result<&'a mut Vec<Operation>> {
        stages.last_mut().map(|s| &mut s.operations).ok_or_else(|| {
            SitefileError::at(E_VALIDATION, "an operation needs a FROM before it", span)
        })
    }

    for ins in &program.instructions {
        match ins {
            Instruction::From {
                span,
                base,
                alias,
                stage_kw,
            } => {
                let base = match base {
                    FromBase::Scratch => StageBase::Scratch,
                    FromBase::Operand(op) => stage_base(
                        op,
                        &vars,
                        &stages,
                        &all_stage_names,
                        &imports,
                        &crate::language::validate::from_lead(*stage_kw, alias.as_deref()),
                    )?,
                    FromBase::Image { reference, at } => image_base(
                        reference,
                        at.as_ref(),
                        &vars,
                        &crate::language::validate::from_lead(*stage_kw, alias.as_deref()),
                    )?,
                };
                stages.push(Stage {
                    name: alias.clone(),
                    base,
                    operations: Vec::new(),
                    span: span.clone(),
                });
            }
            Instruction::Arg {
                span,
                name,
                ty,
                default_value,
            } => {
                declared_args.insert(name);
                if let Some(given) = build_args.get(name) {
                    bind(
                        &mut vars,
                        &mut var_order,
                        name,
                        given,
                        *ty,
                        span,
                        &arg_source(name),
                    )?;
                } else if let Some(d) = default_value {
                    let raw = evaluate_operand(d, &vars)?;
                    bind(
                        &mut vars,
                        &mut var_order,
                        name,
                        &raw,
                        *ty,
                        &d.span,
                        &format!("ARG {name}"),
                    )?;
                } else {
                    return Err(SitefileError::at(
                        E_MISSING_ARG,
                        format!("ARG {name} has no default; pass --build-arg {name}=<value>"),
                        span,
                    ));
                }
            }
            Instruction::Fragment { name, body, span } => {
                let nodes = crate::language::validate::parse_fragment_body(body, &program.file)?;
                fragments.push(FragmentSpec {
                    name: name.clone(),
                    text: body.text().to_string(),
                    nodes: std::sync::Arc::new(nodes),
                    span: span.clone(),
                });
            }
            Instruction::Let {
                name, ty, value, ..
            } => {
                let raw = evaluate_operand(value, &vars)?;
                bind(
                    &mut vars,
                    &mut var_order,
                    name,
                    &raw,
                    *ty,
                    &value.span,
                    &format!("LET {name}"),
                )?;
            }
            Instruction::Import {
                span,
                name,
                uri,
                as_kind,
                integrity,
                sort,
                schema,
                drafts,
                secrets,
                args,
            } => {
                let raw = evaluate_operand(uri, &vars)?;
                let location = classify_uri(&raw, &uri.span)?;
                let schema_planned = match schema {
                    Some(s) => Some(schema_spec(name, &s.path, &vars, &s.span)?),
                    None => None,
                };
                let drafts_planned = match drafts {
                    Some(d) => Some(drafts_value(name, &d.value, &vars)?),
                    None => None,
                };
                let args = args
                    .iter()
                    .map(|a| {
                        Ok(PlannedArg {
                            name: a.name.clone(),
                            value: evaluate_operand(&a.value, &vars)?,
                            span: a.span.clone(),
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                if *as_kind == Some(ImportKind::Sitefile)
                    && matches!(location, ImportLocation::Remote { .. })
                {
                    return Err(SitefileError::at(
                        E_PROTOCOL,
                        format!("IMPORT {name}: AS sitefile reads a context directory or a git+https:// repository, not a URL"),
                        &uri.span,
                    ));
                }
                if matches!(location, ImportLocation::Local { .. }) && !secrets.is_empty() {
                    return Err(SitefileError::at(
                        E_VALIDATION,
                        format!("IMPORT {name}: SECRET applies to remote imports (https:// or git+https://), not local paths"),
                        &secrets[0].span,
                    ));
                }
                if matches!(location, ImportLocation::Git(_)) && integrity.is_some() {
                    return Err(SitefileError::at(
                        E_VALIDATION,
                        format!("IMPORT {name}: INTEGRITY does not apply to Git imports; pin a commit instead (#<40 hex>[:path])"),
                        span,
                    ));
                }
                if matches!(location, ImportLocation::Local { .. }) && integrity.is_some() {
                    warnings.push(format!(
                        "{}:{}: INTEGRITY on a local import is checked but not recorded in the lockfile",
                        program.file, span.line
                    ));
                }
                let spec = ImportSpec {
                    name: name.clone(),
                    uri: raw,
                    location,
                    as_kind: *as_kind,
                    integrity: integrity.clone(),
                    sort: sort.as_ref().map(|s| ImportSort {
                        key: s.key.clone(),
                        descending: s.descending,
                    }),
                    schema: schema_planned,
                    drafts: drafts_planned,
                    secrets: secrets
                        .iter()
                        .map(|s| SecretRef {
                            id: s.id.clone(),
                            header: s.header.clone(),
                            bearer: s.bearer,
                        })
                        .collect(),
                    args,
                    span: span.clone(),
                };
                // A JavaScript Map keeps the first position of a re-set key (validate() forbids duplicates anyway).
                match imports.iter_mut().find(|i| i.name == *name) {
                    Some(existing) => *existing = spec,
                    None => imports.push(spec),
                }
            }
            Instruction::Copy {
                span,
                source,
                dest,
                from,
                overwrite,
                exclude,
                chmod,
                parents,
            } => {
                let d = normalize_virtual(
                    &evaluate_operand(dest, &vars)?,
                    Some(&dest.span),
                    "COPY destination",
                )?;
                if matches!(source, Source::Inline(_)) && d.trailing_slash {
                    return Err(SitefileError::at(
                        E_PATH,
                        "COPY of an inline body needs a file destination, not a directory",
                        &dest.span,
                    ));
                }
                let source = source_ref(source, &vars, &imports, from.as_ref())?;
                operations(&mut stages, span)?.push(Operation::Copy {
                    source,
                    dest: d.path,
                    dest_is_dir: d.trailing_slash,
                    overwrite: *overwrite,
                    exclude: exclude.clone(),
                    chmod: *chmod,
                    parents: *parents,
                    span: span.clone(),
                });
            }
            Instruction::Render {
                span,
                template,
                dest,
                from,
                escape,
                each,
                overwrite,
                inline_svg,
            } => {
                if let Some(e) = each {
                    let template = source_ref(template, &vars, &imports, from.as_ref())?;
                    operations(&mut stages, span)?.push(Operation::Render {
                        template,
                        dest: String::new(),
                        each: Some(RenderEach {
                            as_name: e.as_name.clone(),
                            list: e.list.clone(),
                            dest: dest.clone(),
                            by: e.by.as_ref().map(|b| b.key.clone()),
                            page: match &e.page {
                                Some(pg) => Some(RenderPage {
                                    size: page_size(&pg.size, &vars)?,
                                    first: pg.first.clone(),
                                }),
                                None => None,
                            },
                        }),
                        escape: *escape,
                        overwrite: *overwrite,
                        inline_svg: *inline_svg,
                        span: span.clone(),
                    });
                    continue;
                }
                let raw = evaluate_operand(dest, &vars)?;
                let d = normalize_virtual(&raw, Some(&dest.span), "RENDER destination")?;
                if d.trailing_slash {
                    return Err(SitefileError::at(
                        E_PATH,
                        format!("RENDER destination '{raw}' must be a file path, not a directory"),
                        &dest.span,
                    ));
                }
                let template = source_ref(template, &vars, &imports, from.as_ref())?;
                operations(&mut stages, span)?.push(Operation::Render {
                    template,
                    dest: d.path,
                    each: None,
                    escape: *escape,
                    overwrite: *overwrite,
                    inline_svg: *inline_svg,
                    span: span.clone(),
                });
            }
            Instruction::Plugin {
                span,
                name,
                uri,
                integrity,
            } => {
                let raw = evaluate_operand(uri, &vars)?;
                if url_scheme(&raw).is_some() {
                    return Err(SitefileError::at(
                        E_PROTOCOL,
                        format!("PLUGIN {name}: plugins must be local files in the build context, not URLs"),
                        &uri.span,
                    ));
                }
                let rel = normalize_local(&raw, Some(&uri.span))?.path;
                if !(rel.ends_with(".mjs") || rel.ends_with(".wasm")) {
                    return Err(SitefileError::at(
                        E_VALIDATION,
                        format!("PLUGIN {name}: '{raw}' must be an ES module (.mjs) or a WebAssembly module (.wasm)"),
                        &uri.span,
                    ));
                }
                plugins.push(PluginSpec {
                    name: name.clone(),
                    path: rel,
                    integrity: integrity.clone(),
                    span: span.clone(),
                });
            }
            Instruction::Tool { span, name } => tools.push(ToolSpec {
                name: name.clone(),
                span: span.clone(),
            }),
            Instruction::Transform {
                span,
                source,
                from,
                plugin,
                dest,
                options,
                each,
                args,
                include,
                exclude,
                overwrite,
                ..
            } => {
                let source = match source {
                    Some(s) => Some(source_ref(s, &vars, &imports, from.as_ref())?),
                    None => None,
                };
                operations(&mut stages, span)?.push(Operation::Transform {
                    source,
                    plugin: plugin.clone(),
                    dest: dest.clone(),
                    options: options
                        .iter()
                        .map(|o| PlanOption {
                            key: o.key.clone(),
                            value: o.value.clone(),
                        })
                        .collect(),
                    each: each.as_ref().map(|e| TransformEach {
                        as_name: e.as_name.clone(),
                        list: e.list.clone(),
                    }),
                    args: args.clone(),
                    include: include.clone(),
                    exclude: exclude.clone(),
                    overwrite: *overwrite,
                    span: span.clone(),
                });
            }
            Instruction::Sitemap {
                span,
                base,
                dest,
                overwrite,
            } => {
                let base =
                    check_base_url(&evaluate_operand(base, &vars)?, "SITEMAP BASE", &base.span)?;
                let dest = file_dest(dest.as_ref(), &vars, "/sitemap.xml", "SITEMAP")?;
                operations(&mut stages, span)?.push(Operation::Sitemap {
                    base,
                    dest,
                    overwrite: *overwrite,
                    span: span.clone(),
                });
            }
            Instruction::Fingerprint {
                span,
                patterns,
                manifest,
                integrity,
                no_cache_headers,
                overwrite,
            } => {
                let patterns = patterns
                    .iter()
                    .map(|o| check_pattern(&evaluate_operand(o, &vars)?, &o.span))
                    .collect::<Result<Vec<_>>>()?;
                let manifest = match manifest {
                    Some(m) => Some(file_dest(Some(m), &vars, "", "FINGERPRINT MANIFEST")?),
                    None => None,
                };
                // `root` is the EXPORT path, filled in below (EXPORT is the last instruction).
                operations(&mut stages, span)?.push(Operation::Fingerprint {
                    patterns,
                    manifest,
                    integrity: *integrity,
                    root: "/".into(),
                    overwrite: *overwrite,
                    span: span.clone(),
                });
                fingerprints.push((span.clone(), !*no_cache_headers));
            }
            Instruction::CheckLinks {
                span,
                warn,
                external,
            } => {
                // `root` and `redirects` are filled in below, once every line is planned.
                operations(&mut stages, span)?.push(Operation::CheckLinks {
                    warn: *warn,
                    external: *external,
                    root: "/".into(),
                    redirects: Vec::new(),
                    span: span.clone(),
                });
            }
            Instruction::Feed {
                span,
                list,
                base,
                link,
                dest,
                title,
                overwrite,
                ..
            } => {
                let base =
                    check_base_url(&evaluate_operand(base, &vars)?, "FEED BASE", &base.span)?;
                let dest = file_dest(dest.as_ref(), &vars, "/feed.xml", "FEED")?;
                let title = match title {
                    Some(t) => evaluate_operand(t, &vars)?,
                    None => base.clone(),
                };
                operations(&mut stages, span)?.push(Operation::Feed {
                    list: list.clone(),
                    base,
                    link: link.clone(),
                    dest,
                    title,
                    overwrite: *overwrite,
                    span: span.clone(),
                });
            }
            Instruction::Redirect {
                span,
                from,
                to,
                status,
                overwrite,
            } => {
                redirects.push(RedirectSpec {
                    from: check_site_path(
                        &evaluate_operand(from, &vars)?,
                        "REDIRECT source",
                        &from.span,
                    )?,
                    to: check_redirect_target(&evaluate_operand(to, &vars)?, &to.span)?,
                    status: *status,
                    overwrite: *overwrite,
                    span: span.clone(),
                });
            }
            Instruction::Header {
                span,
                pattern,
                name,
                value,
                overwrite,
            } => {
                let name = check_header_name(&evaluate_operand(name, &vars)?, &name.span)?;
                let pattern = check_site_path(
                    &evaluate_operand(pattern, &vars)?,
                    "HEADER path pattern",
                    &pattern.span,
                )?;
                let words = value
                    .iter()
                    .map(|v| evaluate_operand(v, &vars))
                    .collect::<Result<Vec<_>>>()?;
                let value = check_header_value(&words.join(" "), &name, &value[0].span)?;
                headers.push(HeaderSpec {
                    pattern,
                    name,
                    value,
                    overwrite: *overwrite,
                    span: span.clone(),
                });
            }
            Instruction::Export {
                path,
                format,
                base,
                at,
                compression,
                key,
                span,
            } => {
                export_path = Some(
                    normalize_virtual(
                        &evaluate_operand(path, &vars)?,
                        Some(&path.span),
                        "EXPORT path",
                    )?
                    .path,
                );
                export_format = *format;
                export_base = match base {
                    Some(b) => Some(evaluate_operand(b, &vars)?),
                    None => None,
                };
                if let Some(at) = at {
                    if *format != ExportFormat::Oci {
                        return Err(SitefileError::at(
                            E_VALIDATION,
                            "EXPORT ... AT applies only to AS oci",
                            &at.span,
                        ));
                    }
                    let raw = evaluate_operand(at, &vars)?;
                    export_root =
                        Some(normalize_virtual(&raw, Some(&at.span), "EXPORT AT path")?.path);
                } else {
                    export_root = None;
                }
                if let Some((v, cspan)) = compression {
                    if !matches!(
                        format,
                        ExportFormat::Oci | ExportFormat::Wbn | ExportFormat::Warc
                    ) {
                        return Err(SitefileError::at(
                            E_VALIDATION,
                            "EXPORT ... COMPRESSION applies only to AS oci, AS wbn and AS warc",
                            cspan,
                        ));
                    }
                    export_compression = Some(*v);
                } else {
                    export_compression = None;
                }
                if let Some((_, kspan)) = key {
                    if *format != ExportFormat::Swbn {
                        return Err(SitefileError::at(
                            E_VALIDATION,
                            "EXPORT ... KEY applies only to AS swbn",
                            kspan,
                        ));
                    }
                }
                if *format == ExportFormat::Swbn && key.is_none() {
                    return Err(SitefileError::at(
                        E_VALIDATION,
                        "EXPORT ... AS swbn needs KEY <secret-id>: the --secret that holds the Ed25519 signing key",
                        span,
                    ));
                }
                export_key = key.as_ref().map(|(k, _)| k.clone());
            }
        }
    }

    let root = export_path.clone().unwrap_or_else(|| "/".into());
    for st in &mut stages {
        for op in &mut st.operations {
            match op {
                Operation::Fingerprint { root: r, .. } => *r = root.clone(),
                Operation::CheckLinks {
                    root: r,
                    redirects: rs,
                    ..
                } => {
                    *r = root.clone();
                    *rs = redirects.iter().map(|x| x.from.clone()).collect();
                }
                _ => {}
            }
        }
    }
    // An unnamed stage can be neither a FROM base nor a COPY --from source; only the last one is useful (EXPORT reads it).
    if let Some((_, earlier)) = stages.split_last() {
        for st in earlier.iter().filter(|st| st.name.is_none()) {
            warnings.push(format!(
                "{}:{}: unnamed stage is not the last one, so its output can never be used; name it with FROM … AS <name>",
                program.file, st.span.line
            ));
        }
    }
    for name in build_args.keys() {
        if !declared_args.contains(name) {
            warnings.push(format!(
                "--build-arg {name} does not match any ARG and was ignored"
            ));
        }
    }
    let Some(export_path) = export_path.filter(|_| !stages.is_empty()) else {
        // validate() guarantees both.
        return Err(SitefileError::plain(
            E_VALIDATION,
            "a Sitefile needs FROM and EXPORT",
        ));
    };
    // FINGERPRINT's Cache-Control lines: unless turned off, or a HEADER already sets Cache-Control.
    let cache_headers = match fingerprints.first() {
        Some((first, _))
            if fingerprints.iter().all(|(_, on)| *on)
                && !headers
                    .iter()
                    .any(|h: &HeaderSpec| h.name.eq_ignore_ascii_case("cache-control")) =>
        {
            Some(first.clone())
        }
        _ => None,
    };
    Ok(Plan {
        file: program.file.clone(),
        stages,
        vars,
        var_order,
        fragments,
        imports,
        plugins,
        tools,
        export_path,
        export_format,
        export_base,
        export_root,
        export_compression,
        export_key,
        redirects,
        headers,
        cache_headers,
        warnings,
        target: None,
    })
}

/// `TO <dest>` of SITEMAP/FEED or `MANIFEST <dest>` of FINGERPRINT (a file path), or the default.
fn file_dest(op: Option<&Operand>, vars: &Env, fallback: &str, what: &str) -> Result<String> {
    let Some(op) = op else {
        return Ok(fallback.to_string());
    };
    let raw = evaluate_operand(op, vars)?;
    let dest = normalize_virtual(&raw, Some(&op.span), &format!("{what} destination"))?;
    if dest.trailing_slash {
        return Err(SitefileError::at(
            E_PATH,
            format!("{what} destination '{raw}' must be a file path, not a directory"),
            &op.span,
        ));
    }
    Ok(dest.path)
}

/// `IMPORT … DRAFTS <value>`: `true` keeps drafts, `false` leaves them out.
fn drafts_value(name: &str, op: &Operand, vars: &Env) -> Result<bool> {
    match evaluate_operand(op, vars)?.as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        raw => Err(SitefileError::at(
            E_VALIDATION,
            format!("IMPORT {name}: DRAFTS must be true or false, got '{raw}'"),
            &op.span,
        )),
    }
}

/// `RENDER … PAGE <n>`: a whole number of items per page, at least 1.
fn page_size(op: &Operand, vars: &Env) -> Result<usize> {
    let raw = evaluate_operand(op, vars)?;
    let b = raw.as_bytes();
    if b.is_empty() || b.len() > 9 || b[0] == b'0' || !b.iter().all(u8::is_ascii_digit) {
        return Err(SitefileError::at(
            E_VALIDATION,
            format!(
                "RENDER … PAGE: '{raw}' is not a page size (a whole number from 1 to 999999999)"
            ),
            &op.span,
        ));
    }
    Ok(raw.parse().expect("digits"))
}

/// `IMPORT … SCHEMA <path>`: a local `.json`, `.yaml` or `.yml` file in the build context.
fn schema_spec(name: &str, op: &Operand, vars: &Env, span: &SourceSpan) -> Result<ImportSchema> {
    let uri = evaluate_operand(op, vars)?;
    if url_scheme(&uri).is_some() {
        return Err(SitefileError::at(
            E_PROTOCOL,
            format!("IMPORT {name}: SCHEMA must be a local file in the build context, not a URL"),
            &op.span,
        ));
    }
    let path = normalize_local(&uri, Some(&op.span))?.path;
    let lower = path.to_ascii_lowercase();
    if ![".json", ".yaml", ".yml"]
        .iter()
        .any(|e| lower.ends_with(e))
    {
        return Err(SitefileError::at(
            E_VALIDATION,
            format!("IMPORT {name}: SCHEMA '{uri}' must be a .json, .yaml or .yml file"),
            &op.span,
        ));
    }
    Ok(ImportSchema {
        uri,
        path,
        span: span.clone(),
    })
}

fn classify_uri(uri: &str, span: &SourceSpan) -> Result<ImportLocation> {
    if crate::resources::git::is_git_uri(uri) {
        return Ok(ImportLocation::Git(crate::resources::git::parse_git_uri(
            uri,
            Some(span),
        )?));
    }
    if let Some(scheme) = url_scheme(uri) {
        let scheme = scheme.to_ascii_lowercase();
        return match scheme.as_str() {
            "https" | "http" => Ok(ImportLocation::Remote { url: uri.to_string() }),
            "file" => Err(SitefileError::at(
                E_PROTOCOL,
                "file: URLs are not allowed; use a path relative to the build context",
                span,
            )),
            _ => Err(SitefileError::at(
                E_PROTOCOL,
                format!("unsupported protocol '{scheme}:' (supported: https, git+https, relative paths)"),
                span,
            )),
        };
    }
    Ok(ImportLocation::Local {
        path: normalize_local(uri, Some(span))?.path,
    })
}

/// `image <ref> [AT <dir>]`: the reference must parse; a reference without a registry host names a local-store image.
fn image_base(
    reference: &Operand,
    at: Option<&Operand>,
    vars: &Env,
    lead: &str,
) -> Result<StageBase> {
    let raw = evaluate_operand(reference, vars)?;
    let local = match crate::refs::parse_ref(&raw) {
        Ok(r) => r.registry.is_none(),
        Err(e) => {
            return Err(SitefileError::at(
                E_VALIDATION,
                format!("{lead} image: {}", e.detail),
                &reference.span,
            ))
        }
    };
    let at = match at {
        Some(op) => Some(
            normalize_virtual(
                &evaluate_operand(op, vars)?,
                Some(&op.span),
                "image directory",
            )?
            .path,
        ),
        None => None,
    };
    Ok(StageBase::Image {
        reference: raw,
        local,
        at,
        lead: lead.to_string(),
        span: reference.span.clone(),
    })
}

/// A bare identifier naming an earlier stage is a stage; anything else is a context-relative directory.
fn stage_base(
    op: &Operand,
    vars: &Env,
    earlier: &[Stage],
    all: &BTreeSet<String>,
    imports: &[ImportSpec],
    lead: &str,
) -> Result<StageBase> {
    let raw = evaluate_operand(op, vars)?;
    if earlier
        .iter()
        .any(|s| s.name.as_deref() == Some(raw.as_str()))
    {
        return Ok(StageBase::Stage { name: raw });
    }
    // `<import>+<stage>`: a stage of a Sitefile imported AS sitefile on an earlier line.
    if let Some((import, _)) = imported_stage(&raw) {
        if imports
            .iter()
            .any(|i| i.name == import && i.as_kind == Some(ImportKind::Sitefile))
        {
            return Ok(StageBase::Stage { name: raw });
        }
    }
    if all.contains(&raw) {
        return Err(SitefileError::at(
            E_UNDEFINED,
            format!("{lead} {raw}: stage '{raw}' is not defined before this line (write ./{raw} for a local directory)"),
            &op.span,
        ));
    }
    Ok(StageBase::Local {
        path: normalize_local(&raw, Some(&op.span))?.path,
    })
}

fn source_ref(
    source: &Source,
    vars: &Env,
    imports: &[ImportSpec],
    from: Option<&StageRef>,
) -> Result<SourceRef> {
    let op = match source {
        Source::Inline(b) => {
            return Ok(SourceRef::Inline {
                text: b.text.clone(),
                delimiter: b.delimiter.clone(),
                body_line: b.body_line,
                span: b.span.clone(),
            })
        }
        Source::Operand(op) => op,
    };
    let raw = evaluate_operand(op, vars)?;
    let span = op.span.clone();
    if let Some(from) = from {
        let path = normalize_virtual(
            &raw,
            Some(&span),
            &format!("path inside stage '{}'", from.name),
        )?
        .path;
        return Ok(SourceRef::Stage {
            name: from.name.clone(),
            path,
            span,
        });
    }
    if url_scheme(&raw).is_some() {
        return Err(SitefileError::at(
            E_VALIDATION,
            format!("cannot read '{raw}' directly; IMPORT it under a name, then COPY name:"),
            &span,
        ));
    }
    if let Some((name, inner)) = import_ref(&raw) {
        if !imports.iter().any(|i| i.name == name) {
            return Err(SitefileError::at(
                E_UNDEFINED,
                format!("'{name}:' does not name an IMPORT; write './{raw}' for a local file"),
                &span,
            ));
        }
        let path = if inner.is_empty() {
            "/".to_string()
        } else {
            normalize_virtual(inner, Some(&span), &format!("path inside import '{name}'"))?.path
        };
        return Ok(SourceRef::Import {
            name: name.to_string(),
            path,
            span,
        });
    }
    Ok(SourceRef::Local {
        path: normalize_local(&raw, Some(&span))?.path,
        span,
    })
}

/// A reference to a stage of an imported Sitefile (`FROM theme+assets`, `COPY --from=theme+assets …`).
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedStageRef {
    /// `theme+assets`
    pub name: String,
    pub import: String,
    pub stage: String,
    /// The instruction that names it.
    pub span: SourceSpan,
}

/// Every reference to a stage of an imported Sitefile, in source order (stage bases, then `--from` sources, per stage).
pub fn imported_stage_refs(stages: &[Stage]) -> Vec<ImportedStageRef> {
    let mut out = Vec::new();
    let mut add = |name: &str, span: &SourceSpan| {
        if let Some((import, stage)) = imported_stage(name) {
            out.push(ImportedStageRef {
                name: name.to_string(),
                import: import.to_string(),
                stage: stage.to_string(),
                span: span.clone(),
            });
        }
    };
    for st in stages {
        if let StageBase::Stage { name } = &st.base {
            add(name, &st.span);
        }
        for op in &st.operations {
            let src = match op {
                Operation::Copy { source, .. } => Some(source),
                Operation::Render { template, .. } => Some(template),
                Operation::Transform { source, .. } => source.as_ref(),
                _ => None,
            };
            if let Some(SourceRef::Stage { name, .. }) = src {
                add(name, op.span());
            }
        }
    }
    out
}

// ---- JSON (the shapes `JSON.stringify` gives the TypeScript objects) ---------

fn span_json(s: &SourceSpan) -> Value {
    json!({ "file": s.file, "line": s.line, "column": s.column })
}

fn operand_json(o: &Operand) -> Value {
    let parts: Vec<Value> = o
        .parts
        .iter()
        .map(|p| match p {
            TemplatePart::Text(t) => json!({ "kind": "text", "value": t }),
            TemplatePart::Var { name, path, span } => {
                json!({ "kind": "var", "name": name, "path": path, "span": span_json(span) })
            }
        })
        .collect();
    json!({ "parts": parts, "span": span_json(&o.span) })
}

/// A JavaScript number as `JSON.stringify` writes it (integers without `.0`, non-finite as null).
fn js_number_json(n: f64) -> Value {
    if n.is_finite() && n.fract() == 0.0 && n.abs() < 9_007_199_254_740_992.0 {
        return Value::from(n as i64);
    }
    serde_json::Number::from_f64(n)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

fn scalar_json(s: &Scalar) -> Value {
    match s {
        Scalar::Num(n) => js_number_json(*n),
        other => other.to_json(),
    }
}

impl SourceRef {
    pub fn to_json(&self) -> Value {
        match self {
            SourceRef::Inline {
                text,
                delimiter,
                body_line,
                span,
            } => {
                json!({ "kind": "inline", "text": text, "delimiter": delimiter, "bodyLine": body_line, "span": span_json(span) })
            }
            SourceRef::Stage { name, path, span } => {
                json!({ "kind": "stage", "name": name, "path": path, "span": span_json(span) })
            }
            SourceRef::Local { path, span } => {
                json!({ "kind": "local", "path": path, "span": span_json(span) })
            }
            SourceRef::Import { name, path, span } => {
                json!({ "kind": "import", "name": name, "path": path, "span": span_json(span) })
            }
        }
    }
}

impl ImportSpec {
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("name".into(), json!(self.name));
        m.insert("uri".into(), json!(self.uri));
        m.insert(
            "location".into(),
            match &self.location {
                ImportLocation::Local { path } => json!({ "kind": "local", "path": path }),
                ImportLocation::Remote { url } => json!({ "kind": "remote", "url": url }),
                ImportLocation::Git(g) => {
                    let mut l = Map::new();
                    l.insert("kind".into(), json!("git"));
                    l.insert("url".into(), json!(g.url));
                    l.insert("repo".into(), json!(g.repo));
                    l.insert("ref".into(), json!(g.git_ref));
                    l.insert("path".into(), json!(g.path));
                    if let Some(c) = &g.commit {
                        l.insert("commit".into(), json!(c));
                    }
                    Value::Object(l)
                }
            },
        );
        if let Some(k) = self.as_kind {
            m.insert("as".into(), json!(k.as_str()));
        }
        if let Some(i) = &self.integrity {
            m.insert("integrity".into(), json!(i));
        }
        if let Some(s) = &self.sort {
            m.insert(
                "sort".into(),
                json!({ "key": s.key, "descending": s.descending }),
            );
        }
        if let Some(s) = &self.schema {
            m.insert(
                "schema".into(),
                json!({ "uri": s.uri, "path": s.path, "span": span_json(&s.span) }),
            );
        }
        if let Some(d) = self.drafts {
            m.insert("drafts".into(), json!(d));
        }
        if !self.secrets.is_empty() {
            m.insert(
                "secrets".into(),
                Value::Array(
                    self.secrets
                        .iter()
                        .map(|s| json!({ "id": s.id, "header": s.header, "bearer": s.bearer }))
                        .collect(),
                ),
            );
        }
        if !self.args.is_empty() {
            m.insert(
                "args".into(),
                Value::Array(self.args.iter().map(|a| json!({ "name": a.name, "value": a.value, "span": span_json(&a.span) })).collect()),
            );
        }
        m.insert("span".into(), span_json(&self.span));
        Value::Object(m)
    }
}

fn escape_str(e: EscapeMode) -> &'static str {
    match e {
        EscapeMode::None => "none",
        EscapeMode::Html => "html",
    }
}

impl Operation {
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("kind".into(), json!(self.keyword()));
        match self {
            Operation::CheckLinks {
                warn,
                external,
                root,
                redirects,
                span,
            } => {
                m.insert("warn".into(), json!(warn));
                m.insert("external".into(), json!(external));
                m.insert("root".into(), json!(root));
                m.insert("redirects".into(), json!(redirects));
                m.insert("span".into(), span_json(span));
            }
            Operation::Fingerprint {
                patterns,
                manifest,
                integrity,
                root,
                overwrite,
                span,
            } => {
                m.insert("patterns".into(), json!(patterns));
                if let Some(d) = manifest {
                    m.insert("manifest".into(), json!(d));
                }
                if *integrity {
                    m.insert("integrity".into(), json!(true));
                }
                m.insert("root".into(), json!(root));
                m.insert("overwrite".into(), json!(overwrite));
                m.insert("span".into(), span_json(span));
            }
            Operation::Sitemap {
                base,
                dest,
                overwrite,
                span,
            } => {
                m.insert("base".into(), json!(base));
                m.insert("dest".into(), json!(dest));
                m.insert("overwrite".into(), json!(overwrite));
                m.insert("span".into(), span_json(span));
            }
            Operation::Feed {
                list,
                base,
                link,
                dest,
                title,
                overwrite,
                span,
            } => {
                m.insert("list".into(), json!(list));
                m.insert("base".into(), json!(base));
                m.insert("link".into(), operand_json(link));
                m.insert("dest".into(), json!(dest));
                m.insert("title".into(), json!(title));
                m.insert("overwrite".into(), json!(overwrite));
                m.insert("span".into(), span_json(span));
            }
            Operation::Transform {
                source,
                plugin,
                dest,
                options,
                each,
                args,
                include,
                exclude,
                overwrite,
                span,
            } => {
                if let Some(s) = source {
                    m.insert("source".into(), s.to_json());
                }
                m.insert("plugin".into(), json!(plugin));
                m.insert("dest".into(), operand_json(dest));
                let opts: Vec<Value> = options
                    .iter()
                    .map(|o| json!({ "key": o.key, "value": operand_json(&o.value) }))
                    .collect();
                m.insert("options".into(), Value::Array(opts));
                if let Some(e) = each {
                    m.insert("each".into(), json!({ "as": e.as_name, "list": e.list }));
                }
                if let Some(a) = args {
                    m.insert(
                        "args".into(),
                        Value::Array(a.iter().map(operand_json).collect()),
                    );
                }
                if !include.is_empty() {
                    m.insert("include".into(), json!(include));
                }
                if !exclude.is_empty() {
                    m.insert("exclude".into(), json!(exclude));
                }
                m.insert("overwrite".into(), json!(overwrite));
                m.insert("span".into(), span_json(span));
            }
            Operation::Copy {
                source,
                dest,
                dest_is_dir,
                overwrite,
                exclude,
                chmod,
                parents,
                span,
            } => {
                m.insert("source".into(), source.to_json());
                m.insert("dest".into(), json!(dest));
                m.insert("destIsDir".into(), json!(dest_is_dir));
                m.insert("overwrite".into(), json!(overwrite));
                if !exclude.is_empty() {
                    m.insert("exclude".into(), json!(exclude));
                }
                if let Some(c) = chmod {
                    m.insert("chmod".into(), json!(c));
                }
                if *parents {
                    m.insert("parents".into(), json!(true));
                }
                m.insert("span".into(), span_json(span));
            }
            Operation::Render {
                template,
                dest,
                each,
                escape,
                overwrite,
                inline_svg,
                span,
            } => {
                m.insert("template".into(), template.to_json());
                m.insert("dest".into(), json!(dest));
                if let Some(e) = each {
                    let mut each = Map::new();
                    each.insert("as".into(), json!(e.as_name));
                    each.insert("list".into(), json!(e.list));
                    each.insert("dest".into(), operand_json(&e.dest));
                    if let Some(b) = &e.by {
                        each.insert("by".into(), json!(b));
                    }
                    if let Some(pg) = &e.page {
                        let mut page = Map::new();
                        page.insert("size".into(), json!(pg.size));
                        if let Some(f) = &pg.first {
                            page.insert("first".into(), operand_json(f));
                        }
                        each.insert("page".into(), Value::Object(page));
                    }
                    m.insert("each".into(), Value::Object(each));
                }
                m.insert("escape".into(), json!(escape_str(*escape)));
                m.insert("overwrite".into(), json!(overwrite));
                if *inline_svg {
                    m.insert("inlineSvg".into(), json!(true));
                }
                m.insert("span".into(), span_json(span));
            }
        }
        Value::Object(m)
    }
}

impl Stage {
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        if let Some(n) = &self.name {
            m.insert("name".into(), json!(n));
        }
        m.insert(
            "base".into(),
            match &self.base {
                StageBase::Scratch => json!({ "kind": "scratch" }),
                StageBase::Local { path } => json!({ "kind": "local", "path": path }),
                StageBase::Stage { name } => json!({ "kind": "stage", "name": name }),
                StageBase::Image {
                    reference,
                    local,
                    at,
                    lead,
                    span,
                } => {
                    let mut b = Map::new();
                    b.insert("kind".into(), json!("image"));
                    b.insert("ref".into(), json!(reference));
                    b.insert("local".into(), json!(local));
                    if let Some(a) = at {
                        b.insert("at".into(), json!(a));
                    }
                    b.insert("lead".into(), json!(lead));
                    b.insert("span".into(), span_json(span));
                    Value::Object(b)
                }
            },
        );
        m.insert(
            "operations".into(),
            Value::Array(self.operations.iter().map(Operation::to_json).collect()),
        );
        m.insert("span".into(), span_json(&self.span));
        Value::Object(m)
    }
}

impl Plan {
    /// Variables in declaration order.
    pub fn ordered_vars(&self) -> impl Iterator<Item = (&str, &Scalar)> {
        self.var_order
            .iter()
            .filter_map(|n| self.vars.get(n).map(|v| (n.as_str(), v)))
    }

    /// The object `sitefile validate --json` prints: `{file, vars, imports, stages, export}` (and `target` with `--target`).
    pub fn validate_json(&self) -> Value {
        let mut vars = Map::new();
        for (k, v) in self.ordered_vars() {
            vars.insert(k.to_string(), scalar_json(v));
        }
        let mut v = json!({
            "file": self.file,
            "vars": Value::Object(vars),
            "imports": self.imports.iter().map(ImportSpec::to_json).collect::<Vec<_>>(),
            "stages": self.stages.iter().map(Stage::to_json).collect::<Vec<_>>(),
            "export": self.export_path,
        });
        if let (Some(t), Value::Object(m)) = (&self.target, &mut v) {
            m.insert("target".into(), json!(t));
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;
    use crate::language::validate::parse_program;

    fn args(pairs: &[(&str, &str)]) -> BuildArgs {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn plan_of(src: &str, a: &[(&str, &str)]) -> Result<Plan> {
        plan(&parse_program(src, "Sitefile")?, &args(a))
    }

    fn plan_err(src: &str, a: &[(&str, &str)], code: ErrorCode, line: u32, contains: &str) {
        let e = plan_of(src, a).expect_err(src);
        assert_eq!(e.code, code, "{src}: {e}");
        assert_eq!(e.span.as_ref().map(|s| s.line), Some(line), "{src}: {e}");
        assert!(e.detail.contains(contains), "{src}: {e} lacks {contains:?}");
    }

    #[test]
    fn hello_plan() {
        let p = plan_of(
            "FROM scratch\nARG TITLE = \"Hello\"\nLET GREETING = \"Welcome to ${TITLE}.\"\nCOPY ./public/ /\nRENDER --escape=html ./templates/index.html TO /index.html\nEXPORT / AS directory",
            &[("TITLE", "Docs")],
        )
        .unwrap();
        assert_eq!(
            p.vars.get("GREETING"),
            Some(&Scalar::Str("Welcome to Docs.".into()))
        );
        assert_eq!(
            p.ordered_vars().map(|(k, _)| k).collect::<Vec<_>>(),
            ["TITLE", "GREETING"]
        );
        assert_eq!(p.stages.len(), 1);
        let ops = &p.stages[0].operations;
        assert!(
            matches!(&ops[0], Operation::Copy { source: SourceRef::Local { path, .. }, dest, dest_is_dir: true, .. } if path == "public" && dest == "/")
        );
        assert!(
            matches!(&ops[1], Operation::Render { template: SourceRef::Local { path, .. }, dest, escape: EscapeMode::Html, .. } if path == "templates/index.html" && dest == "/index.html")
        );
        assert_eq!(
            (p.export_path.as_str(), p.export_format),
            ("/", ExportFormat::Directory)
        );
        assert!(p.warnings.is_empty());
    }

    #[test]
    fn typed_args_and_missing_args() {
        let p = plan_of(
            "FROM scratch\nARG N: number = 3\nARG B: boolean = false\nEXPORT /",
            &[("N", "7")],
        )
        .unwrap();
        assert_eq!(p.vars.get("N"), Some(&Scalar::Num(7.0)));
        assert_eq!(p.vars.get("B"), Some(&Scalar::Bool(false)));
        plan_err(
            "FROM scratch\nARG NAME\nEXPORT /",
            &[],
            E_MISSING_ARG,
            2,
            "ARG NAME has no default; pass --build-arg NAME=<value>",
        );
        plan_err(
            "FROM scratch\nARG N: number\nEXPORT /",
            &[("N", "abc")],
            E_TYPE,
            2,
            "--build-arg N expects a number, got 'abc'",
        );
        plan_err(
            "FROM scratch\nLET B: boolean = yes\nEXPORT /",
            &[],
            E_TYPE,
            2,
            "LET B expects true or false",
        );
    }

    #[test]
    fn unknown_build_args_warn() {
        let p = plan_of(
            "FROM scratch\nARG A = 1\nEXPORT /",
            &[("EXTRA", "x"), ("A", "2")],
        )
        .unwrap();
        assert_eq!(
            p.warnings,
            ["--build-arg EXTRA does not match any ARG and was ignored"]
        );
    }

    #[test]
    fn implicit_from_and_unnamed_stage_warnings() {
        let p = plan_of("COPY ./a /a\nEXPORT /", &[]).unwrap();
        assert_eq!(p.stages.len(), 1);
        assert!(matches!(p.stages[0].base, StageBase::Scratch) && p.stages[0].name.is_none());
        assert!(p.warnings.is_empty());

        let p = plan_of("ARG T = \"x\"\nLET U = \"${T}!\"\nEXPORT /", &[("T", "y")]).unwrap();
        assert_eq!(p.vars.get("U"), Some(&Scalar::Str("y!".into())));
        assert_eq!((p.stages.len(), p.stages[0].span.line), (1, 2));

        let p = plan_of(
            "COPY ./a /a\nFROM scratch AS two\nCOPY ./b /b\nEXPORT /",
            &[],
        )
        .unwrap();
        assert_eq!(p.stages.len(), 2);
        assert_eq!(p.warnings, ["Sitefile:1: unnamed stage is not the last one, so its output can never be used; name it with FROM … AS <name>"]);

        assert!(plan_of("FROM scratch\nEXPORT /", &[])
            .unwrap()
            .warnings
            .is_empty());
        assert!(plan_of("FROM scratch AS a\nFROM a\nEXPORT /", &[])
            .unwrap()
            .warnings
            .is_empty());
        let w = plan_of(
            "FROM scratch\nFROM scratch\nFROM scratch AS c\nFROM c\nEXPORT /",
            &[("Z", "1")],
        )
        .unwrap()
        .warnings;
        assert_eq!(w.len(), 3);
        assert!(
            w[0].starts_with("Sitefile:1: unnamed stage")
                && w[1].starts_with("Sitefile:2: unnamed stage")
                && w[2].contains("--build-arg Z")
        );
    }

    #[test]
    fn imports_and_sources() {
        let digest = format!("sha256:{}", "a".repeat(64));
        let p = plan_of(
            &format!("FROM scratch\nARG S = demo\nIMPORT site FROM ./site/\nIMPORT d FROM https://example.com/${{S}}.json AS json INTEGRITY {digest}\nIMPORT l FROM ./l.txt INTEGRITY {digest}\nCOPY site: /\nCOPY site:/img//a.png /a.png\nCOPY ./x:y /z\nEXPORT /out/"),
            &[],
        )
        .unwrap();
        assert_eq!(
            p.imports[0].location,
            ImportLocation::Local {
                path: "site".into()
            }
        );
        assert_eq!(
            p.imports[1].location,
            ImportLocation::Remote {
                url: "https://example.com/demo.json".into()
            }
        );
        assert_eq!(p.imports[1].uri, "https://example.com/demo.json");
        assert_eq!(p.warnings, ["Sitefile:5: INTEGRITY on a local import is checked but not recorded in the lockfile"]);
        let ops = &p.stages[0].operations;
        assert!(
            matches!(&ops[0], Operation::Copy { source: SourceRef::Import { name, path, .. }, .. } if name == "site" && path == "/")
        );
        assert!(
            matches!(&ops[1], Operation::Copy { source: SourceRef::Import { path, .. }, dest_is_dir: false, .. } if path == "/img/a.png")
        );
        assert!(
            matches!(&ops[2], Operation::Copy { source: SourceRef::Local { path, .. }, .. } if path == "x:y")
        );
        assert_eq!(p.export_path, "/out");
    }

    #[test]
    fn protocol_and_path_errors() {
        plan_err(
            "FROM scratch\nIMPORT x FROM ftp://example.com/a\nEXPORT /",
            &[],
            E_PROTOCOL,
            2,
            "unsupported protocol 'ftp:' (supported: https, git+https, relative paths)",
        );
        plan_err(
            "FROM scratch\nIMPORT x FROM FILE:///etc/passwd\nEXPORT /",
            &[],
            E_PROTOCOL,
            2,
            "file: URLs are not allowed",
        );
        plan_err(
            "FROM scratch\nIMPORT x FROM ../up\nEXPORT /",
            &[],
            E_PATH_ESCAPE,
            2,
            "escapes the build context",
        );
        plan_err(
            "FROM scratch\nARG U = https://x/a\nCOPY ${U} /a\nEXPORT /",
            &[],
            E_VALIDATION,
            3,
            "cannot read 'https://x/a' directly; IMPORT it under a name, then COPY name:",
        );
        plan_err(
            "FROM scratch\nARG U = nope:a\nCOPY ${U} /a\nEXPORT /",
            &[],
            E_UNDEFINED,
            3,
            "'nope:' does not name an IMPORT; write './nope:a' for a local file",
        );
        plan_err(
            "FROM scratch\nCOPY a b\nEXPORT /",
            &[],
            E_PATH,
            2,
            "COPY destination 'b' must be an absolute artifact path",
        );
        plan_err(
            "FROM scratch\nRENDER ./t TO /d/\nEXPORT /",
            &[],
            E_PATH,
            2,
            "RENDER destination '/d/' must be a file path, not a directory",
        );
        plan_err(
            "FROM scratch\nCOPY <<E /dir/\nx\nE\nEXPORT /",
            &[],
            E_PATH,
            2,
            "COPY of an inline body needs a file destination",
        );
        plan_err(
            "FROM scratch\nEXPORT / AT /srv",
            &[],
            E_VALIDATION,
            2,
            "EXPORT ... AT applies only to AS oci",
        );
        plan_err(
            "FROM scratch\nEXPORT / AS zip COMPRESSION none",
            &[],
            E_VALIDATION,
            2,
            "EXPORT ... COMPRESSION applies only to AS oci, AS wbn and AS warc",
        );
        let p = plan_of(
            "FROM scratch\nEXPORT / AS wbn BASE https://x.test/ COMPRESSION gzip",
            &[],
        )
        .unwrap();
        assert_eq!(p.export_compression, Some(OciCompression::Gzip));
        let p = plan_of("FROM scratch\nEXPORT / AS oci COMPRESSION none", &[]).unwrap();
        assert_eq!(p.export_compression, Some(OciCompression::None));
        assert_eq!(
            plan_of("FROM scratch\nEXPORT / AS oci", &[])
                .unwrap()
                .export_compression,
            None
        );
    }

    #[test]
    fn plugins_and_tools() {
        let p = plan_of("FROM scratch\nPLUGIN a FROM ./plugins/a.mjs\nPLUGIN w FROM plugins/./w.wasm\nTOOL t\nTRANSFORM ./x WITH a TO /x OPTIONS k=v\nTRANSFORM ./y WITH t TO /y ARGS {input} {output}\nEXPORT /", &[]).unwrap();
        assert_eq!(
            p.plugins
                .iter()
                .map(|x| x.path.as_str())
                .collect::<Vec<_>>(),
            ["plugins/a.mjs", "plugins/w.wasm"]
        );
        assert_eq!(p.tools[0].name, "t");
        let ops = &p.stages[0].operations;
        assert!(
            matches!(&ops[0], Operation::Transform { source: Some(SourceRef::Local { path, .. }), options, args: None, .. } if path == "x" && options[0].key == "k")
        );
        assert!(matches!(&ops[1], Operation::Transform { args: Some(a), .. } if a.len() == 2));
        plan_err(
            "FROM scratch\nPLUGIN a FROM https://x/a.mjs\nEXPORT /",
            &[],
            E_PROTOCOL,
            2,
            "PLUGIN a: plugins must be local files in the build context, not URLs",
        );
        plan_err(
            "FROM scratch\nPLUGIN a FROM ./a.js\nEXPORT /",
            &[],
            E_VALIDATION,
            2,
            "PLUGIN a: './a.js' must be an ES module (.mjs) or a WebAssembly module (.wasm)",
        );
    }

    #[test]
    fn from_image_bases() {
        let p = plan_of("ARG V=2\nSTAGE t FROM image ghcr.io/me/theme:${V} AT /srv//www/\nFROM image theme:1 AS l\nFROM t\nEXPORT /", &[]).unwrap();
        match &p.stages[0].base {
            StageBase::Image {
                reference,
                local,
                at,
                lead,
                span,
            } => {
                assert_eq!(
                    (
                        reference.as_str(),
                        *local,
                        at.as_deref(),
                        lead.as_str(),
                        span.line,
                        span.column
                    ),
                    (
                        "ghcr.io/me/theme:2",
                        false,
                        Some("/srv/www"),
                        "STAGE t FROM",
                        2,
                        20
                    )
                );
            }
            b => panic!("{b:?}"),
        }
        assert!(
            matches!(&p.stages[1].base, StageBase::Image { local: true, at: None, lead, .. } if lead == "FROM")
        );
        assert_eq!(p.stages[2].base, StageBase::Stage { name: "t".into() });
        plan_err(
            "FROM image Bad:1\nEXPORT /",
            &[],
            E_VALIDATION,
            1,
            "FROM image: invalid image reference 'Bad:1'",
        );
        plan_err(
            "FROM image t:1 AT rel\nEXPORT /",
            &[],
            E_PATH,
            1,
            "image directory 'rel' must be an absolute artifact path",
        );
    }

    #[test]
    fn stages_and_from() {
        let p = plan_of("FROM ./base AS a\nCOPY ./x /x\nFROM a AS b\nCOPY --from=a /x /y\nFROM scratch\nCOPY --from=b /y/ /\nEXPORT /", &[]).unwrap();
        assert_eq!(p.stages.len(), 3);
        assert_eq!(
            p.stages[0].base,
            StageBase::Local {
                path: "base".into()
            }
        );
        assert_eq!(p.stages[1].base, StageBase::Stage { name: "a".into() });
        assert_eq!(p.stages[2].base, StageBase::Scratch);
        assert!(
            matches!(&p.stages[1].operations[0], Operation::Copy { source: SourceRef::Stage { name, path, .. }, .. } if name == "a" && path == "/x")
        );
        // a stage name reached through a variable is still checked
        plan_err(
            "ARG S = later\nFROM ${S}\nFROM scratch AS later\nEXPORT /",
            &[],
            E_UNDEFINED,
            2,
            "FROM later: stage 'later' is not defined before this line",
        );
        plan_err(
            "ARG S = later\nSTAGE x FROM ${S}\nSTAGE later\nEXPORT /",
            &[],
            E_UNDEFINED,
            2,
            "STAGE x FROM later: stage 'later' is not defined before this line",
        );
        let p = plan_of("STAGE a\nSTAGE b FROM a\nFROM scratch\nEXPORT /", &[]).unwrap();
        assert_eq!(
            p.stages.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
            [Some("a".to_string()), Some("b".to_string()), None]
        );
        let p = plan_of("ARG D = .\nFROM ${D}\nEXPORT /", &[]).unwrap();
        assert_eq!(p.stages[0].base, StageBase::Local { path: "".into() });
    }

    #[test]
    fn render_for_and_inline() {
        let p = plan_of("FROM scratch\nIMPORT posts FROM ./p AS markdown SORT BY date DESC\nRENDER ./t TO /p/${post.slug}.html FOR post IN posts\nRENDER <<T TO /i.html\nhi\nT\nEXPORT / AS oci AT /srv/www/", &[]).unwrap();
        assert_eq!(
            p.imports[0].sort,
            Some(ImportSort {
                key: "date".into(),
                descending: true
            })
        );
        let ops = &p.stages[0].operations;
        assert!(
            matches!(&ops[0], Operation::Render { dest, each: Some(e), .. } if dest.is_empty() && e.as_name == "post" && e.list == "posts")
        );
        assert!(
            matches!(&ops[1], Operation::Render { template: SourceRef::Inline { text, delimiter, body_line: 5, .. }, .. } if text == "hi\n" && delimiter == "T")
        );
        assert_eq!(
            (p.export_format, p.export_root.as_deref()),
            (ExportFormat::Oci, Some("/srv/www"))
        );
    }

    #[test]
    fn site_instructions() {
        let p = plan_of("ARG URL = \"https://x.example/\"\nSITEMAP BASE ${URL}\nREDIRECT /a /b\nHEADER /* X-A: 1\nEXPORT /", &[]).unwrap();
        assert_eq!(
            p.stages[0].operations[0],
            Operation::Sitemap {
                base: "https://x.example".into(),
                dest: "/sitemap.xml".into(),
                overwrite: false,
                span: SourceSpan::new("Sitefile", 2, 1)
            }
        );
        assert_eq!(
            (
                p.redirects[0].from.as_str(),
                p.redirects[0].to.as_str(),
                p.redirects[0].status
            ),
            ("/a", "/b", 301)
        );
        assert_eq!(
            (
                p.headers[0].pattern.as_str(),
                p.headers[0].name.as_str(),
                p.headers[0].value.as_str()
            ),
            ("/*", "X-A", "1")
        );
        let j = p.stages[0].operations[0].to_json();
        assert_eq!(
            j.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["kind", "base", "dest", "overwrite", "span"]
        );
        let cases: [(&str, ErrorCode, &str); 16] = [
            (
                "SITEMAP BASE ftp://x",
                E_VALIDATION,
                "must be an absolute http(s) URL",
            ),
            (
                "SITEMAP BASE https://u@x",
                E_VALIDATION,
                "must be an absolute http(s) URL",
            ),
            (
                "SITEMAP BASE https://x/?q",
                E_VALIDATION,
                "must be an absolute http(s) URL",
            ),
            (
                "SITEMAP BASE https://",
                E_VALIDATION,
                "must be an absolute http(s) URL",
            ),
            (
                "SITEMAP BASE https://x TO /maps/",
                E_PATH,
                "must be a file path",
            ),
            (
                "SITEMAP BASE https://x TO maps.xml",
                E_PATH,
                "must be an absolute artifact path",
            ),
            (
                "REDIRECT old /b",
                E_VALIDATION,
                "REDIRECT source 'old' must be an absolute path",
            ),
            (
                "REDIRECT \"/a b\" /b",
                E_VALIDATION,
                "must not contain spaces or control characters",
            ),
            (
                "REDIRECT /a \"/b\\nX-Evil: 1\"",
                E_VALIDATION,
                "REDIRECT target must not contain",
            ),
            (
                "REDIRECT /a b",
                E_VALIDATION,
                "must be an absolute path or an http(s) URL",
            ),
            (
                "REDIRECT /a javascript:alert(1)",
                E_VALIDATION,
                "must be an absolute path or an http(s) URL",
            ),
            (
                "HEADER /* \"X A:\" 1",
                E_VALIDATION,
                "HEADER name must not contain spaces",
            ),
            (
                "HEADER /* X(A): 1",
                E_VALIDATION,
                "HEADER name 'X(A)' is not a valid header name",
            ),
            (
                "HEADER /* X-A: \"1\\nSet-Cookie: a=b\"",
                E_VALIDATION,
                "must not contain control characters",
            ),
            ("HEADER /* X-A: \" \"", E_VALIDATION, "the value is empty"),
            (
                "HEADER assets/* X-A: 1",
                E_VALIDATION,
                "HEADER path pattern",
            ),
        ];
        for (src, code, contains) in cases {
            plan_err(&format!("{src}\nEXPORT /"), &[], code, 1, contains);
        }
        let p = plan_of("IMPORT posts FROM ./p AS markdown\nFEED posts BASE https://x/ LINK /p/${item.slug}.html\nEXPORT /", &[]).unwrap();
        let j = p.stages[0].operations[0].to_json();
        assert_eq!(
            j.as_object().unwrap().keys().collect::<Vec<_>>(),
            [
                "kind",
                "list",
                "base",
                "link",
                "dest",
                "title",
                "overwrite",
                "span"
            ]
        );
        assert_eq!(
            (j["dest"].as_str(), j["title"].as_str()),
            (Some("/feed.xml"), Some("https://x"))
        );
    }

    #[test]
    fn validate_json_shape() {
        let p = plan_of("FROM scratch AS s\nARG N: number = 3\nARG F: number = 1.5\nIMPORT d FROM ./d.json AS json\nCOPY --overwrite d: /d.json\nRENDER <<T TO /a\nx\nT\nEXPORT /", &[]).unwrap();
        let v = p.validate_json();
        let text = serde_json::to_string(&v).unwrap();
        assert!(text.starts_with(r#"{"file":"Sitefile","vars":{"N":3,"F":1.5},"imports":[{"name":"d","uri":"./d.json","location":{"kind":"local","path":"d.json"},"as":"json","span":{"file":"Sitefile","line":4,"column":1}}],"stages":[{"name":"s","base":{"kind":"scratch"},"operations":[{"kind":"COPY","source":{"kind":"import","name":"d","path":"/","span":{"file":"Sitefile","line":5,"column":18}},"dest":"/d.json","destIsDir":false,"overwrite":true,"#), "{text}");
        assert!(text.contains(r#"{"kind":"RENDER","template":{"kind":"inline","text":"x\n","delimiter":"T","bodyLine":7,"span":{"file":"Sitefile","line":6,"column":8}},"dest":"/a","escape":"none","overwrite":false,"#), "{text}");
        assert!(text.ends_with(r#""export":"/"}"#), "{text}");
    }

    #[test]
    fn target_plan_keeps_stages_up_to_the_target() {
        let p = plan_of("STAGE a\nCOPY ./x /x\nSTAGE b FROM a\nREDIRECT /o /n\nHEADER /* X-A: 1\nEXPORT /site AS zip", &[]).unwrap();
        let t = target_plan(p.clone(), "a").unwrap();
        assert_eq!(
            t.stages
                .iter()
                .map(|s| s.name.as_deref())
                .collect::<Vec<_>>(),
            [Some("a")]
        );
        assert_eq!(
            (t.export_path.as_str(), t.export_format),
            ("/", ExportFormat::Zip)
        );
        assert!(t.redirects.is_empty() && t.headers.is_empty());
        assert_eq!(t.target.as_deref(), Some("a"));
        assert!(serde_json::to_string(&t.validate_json())
            .unwrap()
            .ends_with(r#""export":"/","target":"a"}"#));
        assert_eq!(target_plan(p.clone(), "b").unwrap().stages.len(), 2);
        let e = target_plan(p, "c").unwrap_err();
        assert_eq!(
            (e.code, e.detail.as_str()),
            (
                ErrorCode::E_USAGE,
                "--target 'c' does not name a stage (stages: a, b)"
            )
        );
        let unnamed = plan_of("FROM scratch\nEXPORT /", &[]).unwrap();
        assert!(target_plan(unnamed, "a")
            .unwrap_err()
            .detail
            .contains("the Sitefile has no named stages"));
    }
}

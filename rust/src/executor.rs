//! Run a build (port of `src/executor.ts`): read the Sitefile, resolve the
//! imports, execute every stage's operations and export the final tree.
//!
//! Messages and warnings match the TypeScript implementation word for word;
//! `../conformance` checks it.

use crate::base_image::{read_layer_blobs, resolve_base, ResolveBaseOptions};
use crate::build_args::BuildArgs;
use crate::registry::RegistryOptions;
use crate::store::Store;
use crate::digest::sha256;
use crate::documents::{data_collection, markdown_collection, sort_documents, DataKind};
use crate::error::{ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::export::archive_writers::{source_date_epoch, WriterOptions};
use crate::export::exporter::{export_directory, export_file, is_exporter_temp_name, resolve_path, ExportOptions};
use crate::export::formats::{encode_tree, is_archive_format, EncodeOptions};
use crate::export::oci::{build_oci_image, image_meta, layout_tree, oci_archive, BaseImage, OciOptions};
use crate::language::ast::{ExportFormat, ImportKind, OciCompression, Operand, Program, TemplatePart};
use crate::language::validate::parse_program;
use crate::paths::{basename_virtual, join_virtual, normalize_virtual};
use crate::planner::{plan, ImportLocation, ImportSpec, Operation, Plan, PluginSpec, SourceRef, Stage, StageBase};
use crate::plugins::cache::{cache_key, digest_folder, CacheStats, TransformCache};
use crate::plugins::exec::{resolve_tool, run_tool, ResolvedTool, ToolCall, ToolInput, ToolPolicy, DEFAULT_TOOL_POLICY};
use crate::plugins::host::{PluginHost, PluginInput, PluginPolicy, PluginRequest, PluginResult, DEFAULT_PLUGIN_POLICY};
use crate::plugins::wasm::check_wasm_plugin;
use crate::render::{decode_utf8, parse_render_template, render_template, resolve_json_path, walk};
use crate::resources::convert::{bytes_to_resource, verify_integrity, BytesInfo};
use crate::resources::https::fetch_remote;
use crate::resources::local::{EntryKind, LocalContext, LocalContextOptions};
use crate::resources::lockfile::{LockEntry, Lockfile};
use crate::resources::types::{
    ArchiveLimits, FetchPolicy, Resource, ValueFormat, DEFAULT_ARCHIVE_LIMITS, DEFAULT_FETCH_POLICY,
};
use crate::tree::{make_file, ConflictPolicy, FileNode, Node, Tree};
use crate::values::{Env, Scalar};
use serde_json::{json, Map, Value};
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// A message callback (warnings, notices). Shared with plugin runners and import threads.
pub type Callback = Arc<dyn Fn(&str) + Send + Sync>;

/// Where Sitefile.lock lives.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum LockfileSetting {
    /// `Sitefile.lock` next to the Sitefile.
    #[default]
    Default,
    Path(PathBuf),
    /// `--no-lockfile`.
    Disabled,
}

#[derive(Clone)]
pub struct BuildOptions {
    /// Build context directory, as given (messages show it this way).
    pub context_dir: String,
    /// Sitefile path. Default: `<context_dir>/Sitefile`.
    pub sitefile: Option<String>,
    pub build_args: BuildArgs,
    /// Export destination; skipped when walking the context if it lies inside it.
    pub out_dir: Option<String>,
    pub fetch_policy: FetchPolicy,
    /// Limits for extracting archives imported AS tree.
    pub archive_limits: ArchiveLimits,
    pub lockfile: LockfileSetting,
    /// Fail instead of adding or changing lockfile entries.
    pub frozen_lockfile: bool,
    /// Re-record unpinned URLs whose content changed.
    pub update_lock: bool,
    /// Maximum imports resolved at once (default 4).
    pub concurrency: usize,
    pub plugin_policy: PluginPolicy,
    /// Permit hard-linked files in the context.
    pub allow_hardlinks: bool,
    /// `--allow-exec` entries: `name` or `name=/absolute/path`.
    pub allow_exec: Vec<String>,
    /// `--allow-plugin` entries: JavaScript PLUGINs allowed to run, by declared name.
    /// WebAssembly plugins need no permission.
    pub allow_plugin: Vec<String>,
    /// `--allow-plugins`: allow every JavaScript PLUGIN.
    pub allow_all_plugins: bool,
    pub tool_policy: ToolPolicy,
    /// Directory of the TRANSFORM result cache; `None` disables caching.
    pub cache_dir: Option<PathBuf>,
    pub on_warning: Option<Callback>,
    /// Informational lines (the path each allowed tool resolved to).
    pub on_notice: Option<Callback>,
    /// oci: `os/arch` of the image (and of the BASE to pull).
    pub platform: Option<String>,
    /// `--base`: overrides the Sitefile's `AS oci BASE`.
    pub base_image: Option<String>,
    /// Do not resolve a BASE.
    pub skip_base: bool,
    /// Registry hosts reachable over plain http (for BASE pulls).
    pub insecure_registries: Vec<String>,
}

impl Default for BuildOptions {
    fn default() -> Self {
        BuildOptions {
            context_dir: ".".into(),
            sitefile: None,
            build_args: BuildArgs::new(),
            out_dir: None,
            fetch_policy: DEFAULT_FETCH_POLICY,
            archive_limits: DEFAULT_ARCHIVE_LIMITS,
            lockfile: LockfileSetting::Default,
            frozen_lockfile: false,
            update_lock: false,
            concurrency: 4,
            plugin_policy: DEFAULT_PLUGIN_POLICY,
            allow_hardlinks: false,
            allow_exec: Vec::new(),
            allow_plugin: Vec::new(),
            allow_all_plugins: false,
            tool_policy: DEFAULT_TOOL_POLICY,
            cache_dir: None,
            on_warning: None,
            on_notice: None,
            platform: None,
            base_image: None,
            skip_base: false,
            insecure_registries: Vec::new(),
        }
    }
}

/// What the TRANSFORMs did: processes started, calls made, cache activity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TransformStats {
    pub cache: CacheStats,
    pub plugins_spawned: u64,
    pub plugin_calls: u64,
    pub tool_runs: u64,
}

pub struct BuildResult {
    pub plan: Plan,
    /// The final stage's whole tree.
    pub tree: Tree,
    /// The exported subtree.
    pub output: Tree,
    /// Imports by name, in declaration order.
    pub resources: Vec<(String, Resource)>,
    pub warnings: Vec<String>,
    pub lockfile_written: bool,
    /// The resolved BASE image.
    pub base: Option<BaseImage>,
    pub tools: Vec<ResolvedTool>,
    pub transform_stats: TransformStats,
}

pub struct Loaded {
    pub program: Program,
    pub sitefile_path: PathBuf,
    pub label: String,
}

/// `path.relative(from, to)` for absolute, normalized paths.
pub fn relative_path(from: &Path, to: &Path) -> String {
    let a: Vec<_> = from.components().collect();
    let b: Vec<_> = to.components().collect();
    let mut i = 0;
    while i < a.len() && i < b.len() && a[i] == b[i] {
        i += 1;
    }
    let mut parts: Vec<String> = Vec::new();
    for _ in i..a.len() {
        parts.push("..".into());
    }
    for c in &b[i..] {
        parts.push(c.as_os_str().to_string_lossy().into_owned());
    }
    parts.join("/")
}

fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
}

/// `path.join(a, b)`: joined and normalized, relative stays relative.
pub fn join_path(a: &str, b: &str) -> String {
    let joined = if a.is_empty() { b.to_string() } else if b.is_empty() { a.to_string() } else { format!("{a}/{b}") };
    normalize_posix(&joined)
}

/// `path.posix.normalize`.
pub fn normalize_posix(p: &str) -> String {
    if p.is_empty() {
        return ".".into();
    }
    let abs = p.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if matches!(out.last(), Some(s) if *s != "..") {
                    out.pop();
                } else if !abs {
                    out.push("..");
                }
            }
            s => out.push(s),
        }
    }
    let body = out.join("/");
    let mut s = if abs { format!("/{body}") } else if body.is_empty() { ".".into() } else { body };
    if p.ends_with('/') && !s.ends_with('/') {
        s.push('/');
    }
    s
}

/// Read, parse and validate a Sitefile.
pub fn load_program(context_dir: &str, sitefile: Option<&str>) -> Result<Loaded> {
    let given = match sitefile {
        Some(s) => s.to_string(),
        None => join_path(context_dir, "Sitefile"),
    };
    let sitefile_path = resolve_path(Path::new(&given));
    let source = std::fs::read(&sitefile_path)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .map_err(|_| SitefileError::plain(E_NOT_FOUND, format!("cannot read Sitefile '{}'", sitefile_path.display())))?;
    let rel = relative_path(&cwd(), &sitefile_path);
    let label = if rel.is_empty() {
        sitefile_path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    } else {
        rel
    };
    Ok(Loaded { program: parse_program(&source, &label)?, sitefile_path, label })
}

/// Parse, validate and evaluate variables, without reading any sources.
pub fn plan_build(options: &BuildOptions) -> Result<Plan> {
    let loaded = load_program(&options.context_dir, options.sitefile.as_deref())?;
    plan(&loaded.program, &options.build_args)
}

type Warn = Callback;

/// realpath of the nearest existing ancestor, with the missing tail re-joined.
fn realpath_loose(p: &Path) -> PathBuf {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = p.to_path_buf();
    loop {
        if let Ok(r) = std::fs::canonicalize(&cur) {
            let mut out = r;
            for t in tail.iter().rev() {
                out.push(t);
            }
            return out;
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                cur = parent.to_path_buf();
            }
            _ => return p.to_path_buf(),
        }
    }
}

/// Run a build in memory.
pub fn build(options: &BuildOptions) -> Result<BuildResult> {
    let Loaded { program, sitefile_path, label } = load_program(&options.context_dir, options.sitefile.as_deref())?;
    let p = plan(&program, &options.build_args)?;
    let warnings: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let warn: Warn = {
        let warnings = warnings.clone();
        let on = options.on_warning.clone();
        Arc::new(move |m: &str| {
            if let Ok(mut w) = warnings.lock() {
                w.push(m.to_string());
            }
            if let Some(f) = &on {
                f(m);
            }
        })
    };
    for w in &p.warnings {
        warn(w);
    }
    let tools = resolve_tools(&p, options, &warn)?;
    let plugin_permission = check_plugins(&p, options, &warn)?;

    let context_real = std::fs::canonicalize(&options.context_dir)
        .map_err(|_| SitefileError::plain(E_NOT_FOUND, format!("build context '{}' does not exist", options.context_dir)))?;
    let mut ignore = vec![".git".to_string()];
    if let Some(out) = &options.out_dir {
        let out_real = realpath_loose(&resolve_path(Path::new(out)));
        let rel = relative_path(&context_real, &out_real);
        if !rel.is_empty() && !rel.starts_with("..") && !rel.starts_with('/') {
            ignore.push(rel);
            // Also skip temp/old siblings an interrupted export may have left behind.
            if let (Some(parent), Some(base)) = (out_real.parent(), out_real.file_name()) {
                let base = base.to_string_lossy().into_owned();
                if let Ok(rd) = std::fs::read_dir(parent) {
                    for e in rd.flatten() {
                        let name = e.file_name().to_string_lossy().into_owned();
                        if is_exporter_temp_name(&name, &base) {
                            ignore.push(relative_path(&context_real, &parent.join(&name)));
                        }
                    }
                }
            }
        }
    }
    let local = LocalContext::open(
        &context_real,
        &LocalContextOptions { ignore: Some(ignore), sitefileignore: true, allow_hardlinks: options.allow_hardlinks },
    )?;
    let lock_path = match &options.lockfile {
        LockfileSetting::Disabled => None,
        LockfileSetting::Path(p) => Some(p.clone()),
        LockfileSetting::Default => Some(sitefile_path.parent().unwrap_or(Path::new("/")).join("Sitefile.lock")),
    };
    let lock = match lock_path {
        Some(p) => Some(Lockfile::load(p)?),
        None => None,
    };
    let lock = Mutex::new(lock);

    let env = ImportEnv {
        local: &local,
        policy: &options.fetch_policy,
        archive_limits: &options.archive_limits,
        lock: &lock,
        label: &label,
        warn: &warn,
        frozen: options.frozen_lockfile,
        update: options.update_lock,
    };
    let resolved = map_limit(&p.imports, options.concurrency.max(1), |spec| resolve_import(spec, &env))?;
    let mut resources: HashMap<String, Resource> = HashMap::new();
    let mut resource_list: Vec<(String, Resource)> = Vec::new();
    for (spec, res) in p.imports.iter().zip(resolved) {
        let res = sorted(spec, res)?;
        resources.insert(spec.name.clone(), res.clone());
        resource_list.push((spec.name.clone(), res));
    }

    let plugin_warn = warn.clone();
    let mut exec = Exec {
        local: &local,
        resources: &resources,
        stages: HashMap::new(),
        vars: &p.vars,
        label: &label,
        warn: warn.clone(),
        plugins: PluginHost::new(options.plugin_policy, move |m: &str| plugin_warn(m)),
        tools: tools.iter().map(|t| (t.name.clone(), t.clone())).collect(),
        tool_policy: options.tool_policy,
        cache: options.cache_dir.as_ref().map(TransformCache::new),
        plugin_digests: HashMap::new(),
        stats: TransformStats::default(),
    };
    let run = (|| -> Result<Tree> {
        for spec in &p.plugins {
            register_plugin(spec, &local, &mut exec.plugins, &plugin_permission)?;
        }
        let mut tree = Tree::empty();
        for stage in &p.stages {
            tree = exec.start_stage(stage)?;
            for op in &stage.operations {
                tree = exec.execute(op, tree)?;
            }
            if let Some(name) = &stage.name {
                exec.stages.insert(name.clone(), tree.clone());
            }
        }
        Ok(tree)
    })();
    exec.plugins.close();
    exec.stats.plugins_spawned = exec.plugins.stats.spawned;
    exec.stats.plugin_calls = exec.plugins.stats.calls;
    if let Some(c) = &exec.cache {
        exec.stats.cache = c.stats;
    }
    let stats = exec.stats;
    drop(exec);
    let tree = run?;

    if tree.is_file(&p.export_path) {
        return Err(SitefileError::plain(E_EXPORT, format!("EXPORT path '{}' is a file; export a directory", p.export_path)));
    }
    let output = tree.subtree(&p.export_path);
    if output.is_empty() {
        warn(&format!("EXPORT {} contains no files", p.export_path));
    }

    // BASE: the image to build on, resolved after the files so a failing build never pulls anything.
    let base_ref = if options.skip_base {
        None
    } else {
        options.base_image.clone().or_else(|| if p.export_format == ExportFormat::Oci { p.export_base.clone() } else { None })
    };
    let mut lock = lock.into_inner().unwrap_or(None);
    let base = match base_ref {
        Some(r) => {
            let store = Store::open_default()?;
            let w = warn.clone();
            let warn_fn = move |m: &str| w(m);
            let o = ResolveBaseOptions {
                platform: options.platform.clone(),
                frozen: options.frozen_lockfile,
                update: options.update_lock,
                registry: RegistryOptions {
                    insecure_registries: options.insecure_registries.clone(),
                    allow_private_network: options.fetch_policy.allow_private_network,
                    ..Default::default()
                },
                warn: Some(&warn_fn),
                ..ResolveBaseOptions::new(&store)
            };
            Some(resolve_base(&r, lock.as_mut(), &o)?)
        }
        None => None,
    };

    let lockfile_written = match lock.as_mut() {
        Some(l) if !options.frozen_lockfile => l.save()?,
        _ => false,
    };
    let warnings = warnings.lock().map(|w| w.clone()).unwrap_or_default();
    Ok(BuildResult { plan: p, tree, output, resources: resource_list, warnings, lockfile_written, base, tools, transform_stats: stats })
}

// ---------------------------------------------------------------- export

/// Overrides for where and how the export is written.
#[derive(Debug, Clone, Default)]
pub struct ExportTarget {
    /// Overrides the Sitefile's EXPORT format.
    pub format: Option<ExportFormat>,
    /// Overrides the Sitefile's EXPORT ... BASE (wbn).
    pub base_url: Option<String>,
    /// oci: root directory inside the layer (overrides EXPORT ... AT).
    pub oci_root: Option<String>,
    /// oci: site layer encoding (overrides EXPORT ... COMPRESSION; default gzip).
    pub oci_compression: Option<OciCompression>,
    /// Extra OCI config labels.
    pub image_labels: std::collections::BTreeMap<String, String>,
    /// Add org.opencontainers.image.source/revision from the git checkout.
    pub git_labels: bool,
    /// oci: `name:tag` recorded as org.opencontainers.image.ref.name in index.json.
    pub oci_ref_name: Option<String>,
    /// Replace a non-empty destination outside the context.
    pub force: bool,
}

pub struct ExportedBuild {
    pub result: BuildResult,
    pub format: ExportFormat,
    /// Real path written: a directory for `directory`, otherwise the archive file.
    pub out_path: PathBuf,
    /// Archive size in bytes (archive formats only).
    pub bytes: Option<usize>,
    /// `sha256:<hex>` of the archive file (archive formats only).
    pub file_digest: Option<String>,
    /// oci: the image's manifest digest.
    pub manifest_digest: Option<String>,
}

/// OCI options for the build's exported tree (shared by `-t` image storage).
pub fn oci_options(
    result: &BuildResult,
    context_dir: &str,
    root: Option<&str>,
    compression: Option<OciCompression>,
    platform: Option<&str>,
    labels: &std::collections::BTreeMap<String, String>,
    git: bool,
) -> Result<OciOptions> {
    let meta = image_meta(Path::new(context_dir), git, labels.clone());
    Ok(OciOptions {
        root: root.map(str::to_string).or_else(|| result.plan.export_root.clone()),
        platform: platform.map(str::to_string),
        epoch: source_date_epoch()?,
        title: meta.title,
        source: meta.source,
        revision: meta.revision,
        labels: meta.labels,
        base: result.base.clone(),
        compression: compression.or(result.plan.export_compression).unwrap_or_default(),
    })
}

/// Build, then write the exported tree to `out_dir`: a directory for the
/// `directory` format, a single file for zip, tar, tgz and wbn; oci writes an
/// OCI archive for `*.tar` and an image layout directory otherwise.
pub fn build_to_directory(options: &BuildOptions, out_dir: &str, target: &ExportTarget) -> Result<ExportedBuild> {
    let result = build(options)?;
    let format = target.format.unwrap_or(result.plan.export_format);
    let export_options = ExportOptions::new(&options.context_dir).force(target.force);
    let out = Path::new(out_dir);
    if format == ExportFormat::Oci {
        let opts = oci_options(&result, &options.context_dir, target.oci_root.as_deref(), target.oci_compression, options.platform.as_deref(), &target.image_labels, target.git_labels)?;
        let img = build_oci_image(&result.output, &opts)?;
        // An image built on a BASE carries the base's layers: copy them in so the export is self-contained.
        let base_blobs = match &result.base {
            Some(_) => read_layer_blobs(&Store::open_default()?, &img.base_layers)?,
            None => Vec::new(),
        };
        let ref_name = target.oci_ref_name.as_deref();
        if out_dir.to_lowercase().ends_with(".tar") {
            let data = oci_archive(img.parts(), ref_name, source_date_epoch()?, &base_blobs)?;
            let written = export_file(&data, out, &export_options)?;
            let digest = sha256(&data);
            return Ok(ExportedBuild {
                result,
                format,
                out_path: written.out_file,
                bytes: Some(written.bytes),
                file_digest: Some(digest),
                manifest_digest: Some(img.manifest_digest.clone()),
            });
        }
        let written = export_directory(&layout_tree(img.parts(), ref_name, &base_blobs)?, out, &export_options)?;
        return Ok(ExportedBuild { result, format, out_path: written.out_dir, bytes: None, file_digest: None, manifest_digest: Some(img.manifest_digest.clone()) });
    }
    if !is_archive_format(format) {
        let written = export_directory(&result.output, out, &export_options)?;
        return Ok(ExportedBuild { result, format, out_path: written.out_dir, bytes: None, file_digest: None, manifest_digest: None });
    }
    let data = encode_tree(
        &result.output,
        format,
        &EncodeOptions {
            writer: WriterOptions { epoch: source_date_epoch()?, dirs: Vec::new() },
            base_url: target.base_url.clone().or_else(|| result.plan.export_base.clone()),
        },
    )?;
    let written = export_file(&data, out, &export_options)?;
    let digest = sha256(&data);
    Ok(ExportedBuild { result, format, out_path: written.out_file, bytes: Some(written.bytes), file_digest: Some(digest), manifest_digest: None })
}

// ---------------------------------------------------------------- imports

struct ImportEnv<'a> {
    local: &'a LocalContext,
    policy: &'a FetchPolicy,
    archive_limits: &'a ArchiveLimits,
    lock: &'a Mutex<Option<Lockfile>>,
    label: &'a str,
    warn: &'a Warn,
    frozen: bool,
    update: bool,
}

/// Run `f` over `items` with at most `limit` at once; results in input order.
/// The first failure (by position) is returned.
fn map_limit<T: Sync, R: Send>(items: &[T], limit: usize, f: impl Fn(&T) -> Result<R> + Sync) -> Result<Vec<R>> {
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let workers = limit.min(items.len()).max(1);
    if workers == 1 {
        return items.iter().map(&f).collect();
    }
    let next = AtomicUsize::new(0);
    let slots: Vec<Mutex<Option<Result<R>>>> = (0..items.len()).map(|_| Mutex::new(None)).collect();
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                if i >= items.len() {
                    break;
                }
                let r = f(&items[i]);
                if let Ok(mut slot) = slots[i].lock() {
                    *slot = Some(r);
                }
            });
        }
    });
    let mut out = Vec::with_capacity(items.len());
    for slot in slots {
        match slot.into_inner().ok().flatten() {
            Some(Ok(r)) => out.push(r),
            Some(Err(e)) => return Err(e),
            None => return Err(SitefileError::plain(E_FETCH, "import worker failed")),
        }
    }
    Ok(out)
}

fn trim_trailing_slashes(s: &str) -> String {
    s.trim_end_matches('/').to_string()
}

fn integrity_on_dir(spec: &ImportSpec) -> SitefileError {
    SitefileError::at(E_VALIDATION, format!("IMPORT {}: INTEGRITY applies to files, not directories", spec.name), &spec.span)
}

/// `path.posix.basename(new URL(url).pathname)`.
fn url_basename(url: &str) -> String {
    let after = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let path = match after.find('/') {
        Some(i) => &after[i..],
        None => "/",
    };
    let path = path.split(['?', '#']).next().unwrap_or("");
    path.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string()
}

fn resolve_import(spec: &ImportSpec, env: &ImportEnv<'_>) -> Result<Resource> {
    let origin = format!("{}:{} IMPORT {}", env.label, spec.span.line, spec.name);
    let span = Some(&spec.span);
    match &spec.location {
        ImportLocation::Local { path: rel } => {
            if env.local.stat(rel, span)? == EntryKind::Directory {
                match spec.as_kind {
                    Some(ImportKind::Markdown) => {
                        if spec.integrity.is_some() {
                            return Err(integrity_on_dir(spec));
                        }
                        let Resource::Tree { tree, provenance } = env.local.read_tree(rel, &origin, span)? else { unreachable!() };
                        let docs = markdown_collection(&tree, &trim_trailing_slashes(&spec.uri), span)?;
                        return Ok(Resource::Value { format: ValueFormat::Markdown, value: Value::Array(docs.into_iter().map(Value::Object).collect()), provenance });
                    }
                    Some(k @ (ImportKind::Json | ImportKind::Yaml)) => {
                        if spec.integrity.is_some() {
                            return Err(integrity_on_dir(spec));
                        }
                        let Resource::Tree { tree, provenance } = env.local.read_tree(rel, &origin, span)? else { unreachable!() };
                        let (kind, format) = if k == ImportKind::Json { (DataKind::Json, ValueFormat::Json) } else { (DataKind::Yaml, ValueFormat::Yaml) };
                        let docs = data_collection(&tree, kind, &trim_trailing_slashes(&spec.uri), span)?;
                        return Ok(Resource::Value { format, value: Value::Array(docs.into_iter().map(Value::Object).collect()), provenance });
                    }
                    Some(k) if k != ImportKind::Tree => {
                        return Err(SitefileError::at(
                            E_TYPE,
                            format!("IMPORT {}: '{}' is a directory and cannot be imported AS {}", spec.name, spec.uri, k.as_str()),
                            &spec.span,
                        ));
                    }
                    _ => {}
                }
                if spec.integrity.is_some() {
                    return Err(integrity_on_dir(spec));
                }
                return env.local.read_tree(rel, &origin, span);
            }
            let file = env.local.read_file(rel, span)?;
            let Resource::File { bytes, mode, basename, .. } = &file else { unreachable!() };
            if let Some(i) = &spec.integrity {
                verify_integrity(bytes, i, &spec.uri, span)?;
            }
            match spec.as_kind {
                None | Some(ImportKind::File) => Ok(file),
                Some(kind) => {
                    let info = BytesInfo { uri: spec.uri.clone(), resolved_url: None, basename: basename.clone(), mode: Some(*mode) };
                    let Resource::File { bytes, .. } = file else { unreachable!() };
                    bytes_to_resource(bytes, kind, &info, span, env.archive_limits)
                }
            }
        }
        ImportLocation::Remote { url } => {
            let kind = spec.as_kind.unwrap_or(ImportKind::File);
            let entry = env.lock.lock().ok().and_then(|l| l.as_ref().and_then(|l| l.get(url).cloned()));
            if env.frozen && entry.is_none() {
                return Err(SitefileError::at(E_LOCK_MISSING, format!("IMPORT {}: '{url}' is not in the lockfile (--frozen-lockfile)", spec.name), &spec.span));
            }
            let fetched = fetch_remote(url, env.policy, span)?;
            let digest = sha256(&fetched.bytes);
            if let Some(i) = &spec.integrity {
                verify_integrity(&fetched.bytes, i, url, span)?;
            } else if let Some(e) = entry.as_ref().filter(|e| !env.update && e.integrity != digest) {
                return Err(SitefileError::at(
                    E_LOCK_MISMATCH,
                    format!(
                        "IMPORT {}: '{url}' changed since it was locked (locked {}, got {digest}); rerun with --update-lock to accept it",
                        spec.name, e.integrity
                    ),
                    &spec.span,
                ));
            } else if entry.is_none() {
                (env.warn)(&format!(
                    "{}:{}: IMPORT {} is not pinned; add INTEGRITY {digest} for a reproducible build",
                    env.label, spec.span.line, spec.name
                ));
            }
            if env.frozen && entry.as_ref().is_some_and(|e| e.integrity != digest) {
                return Err(SitefileError::at(E_LOCK_MISMATCH, format!("IMPORT {}: '{url}' does not match the lockfile (--frozen-lockfile)", spec.name), &spec.span));
            }
            if let Ok(mut l) = env.lock.lock() {
                if let Some(l) = l.as_mut() {
                    l.record(url, LockEntry { integrity: digest, size: fetched.bytes.len() as u64, resolved: fetched.final_url.clone() });
                }
            }
            let base = url_basename(&fetched.final_url);
            let basename = if base.is_empty() { spec.name.clone() } else { base };
            let info = BytesInfo { uri: url.clone(), resolved_url: Some(fetched.final_url.clone()), basename, mode: None };
            bytes_to_resource(fetched.bytes, kind, &info, span, env.archive_limits)
        }
    }
}

/// Apply an import's SORT BY to a list value.
fn sorted(spec: &ImportSpec, res: Resource) -> Result<Resource> {
    let Some(sort) = &spec.sort else { return Ok(res) };
    let Resource::Value { format, value, provenance } = res else { return Ok(res) };
    let Value::Array(items) = value else {
        return Err(SitefileError::at(
            E_TYPE,
            format!("IMPORT {}: SORT BY needs a list (a directory of documents, a CSV file or a JSON/YAML array), not a single value", spec.name),
            &spec.span,
        ));
    };
    if !items.iter().all(Value::is_object) {
        return Err(SitefileError::at(E_TYPE, format!("IMPORT {}: SORT BY needs a list of objects with a '{}' field", spec.name, sort.key), &spec.span));
    }
    Ok(Resource::Value { format, value: Value::Array(sort_documents(items, &sort.key, sort.descending)), provenance })
}

// ---------------------------------------------------------------- plugins and tools

/// Check a plugin file (sandbox, own folder, integrity) and register it.
fn register_plugin(spec: &PluginSpec, local: &LocalContext, host: &mut PluginHost, permission: &PluginPermission) -> Result<()> {
    let span = Some(&spec.span);
    let (real, bytes) = local.real_file(&spec.path, span)?;
    if let Some(i) = &spec.integrity {
        verify_integrity(&bytes, i, &format!("./{}", spec.path), span)?;
    }
    if real.to_string_lossy().ends_with(".wasm") {
        // A wasm plugin has no imports, so it reads nothing: no own-folder rule needed.
        check_wasm_plugin(&spec.name, &bytes, span)?;
        host.register(&spec.name, real, Some(spec.span.clone()), Some(bytes));
        return Ok(());
    }
    // It runs as JavaScript (also a '.wasm' path that is a symlink to a JS file), so it needs the opt-in.
    permission.check(spec)?;
    // A JS plugin may read its whole folder, so that folder must not be the context itself.
    if real.parent() == Some(local.root()) {
        let base = real.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        return Err(SitefileError::at(
            E_VALIDATION,
            format!("PLUGIN {}: put the plugin in its own folder (e.g. ./plugins/{base}); a plugin can read every file next to it", spec.name),
            &spec.span,
        ));
    }
    host.register(&spec.name, real, Some(spec.span.clone()), None);
    Ok(())
}

fn is_identifier(s: &str) -> bool {
    let mut c = s.chars();
    c.next().is_some_and(|f| f.is_ascii_alphabetic() || f == '_') && c.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// Which JavaScript PLUGINs `--allow-plugin` / `--allow-plugins` let run.
struct PluginPermission {
    all: bool,
    names: Vec<String>,
}

impl PluginPermission {
    fn check(&self, spec: &PluginSpec) -> Result<()> {
        if self.all || self.names.contains(&spec.name) {
            return Ok(());
        }
        Err(SitefileError::at(
            E_UNSUPPORTED,
            format!(
                "PLUGIN {0} runs JavaScript, which must be allowed explicitly: add --allow-plugin {0} (or --allow-plugins to allow every PLUGIN); WebAssembly plugins need no flag",
                spec.name
            ),
            &spec.span,
        ))
    }
}

/// Check every JavaScript PLUGIN against `--allow-plugin` / `--allow-plugins` before
/// anything is read or started. A wasm plugin (a module that imports nothing) runs
/// without permission. Returns the check, which register_plugin repeats on the real path.
fn check_plugins(p: &Plan, options: &BuildOptions, warn: &Warn) -> Result<PluginPermission> {
    let mut names: Vec<String> = Vec::new();
    for name in &options.allow_plugin {
        if !is_identifier(name) {
            return Err(SitefileError::plain(E_USAGE, format!("--allow-plugin expects a PLUGIN name (an identifier), got '{name}'")));
        }
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    let permission = PluginPermission { all: options.allow_all_plugins, names };
    for spec in &p.plugins {
        if !spec.path.ends_with(".wasm") {
            permission.check(spec)?;
        }
    }
    for name in &permission.names {
        if !p.plugins.iter().any(|s| &s.name == name) {
            warn(&format!("--allow-plugin {name} does not match any PLUGIN and was ignored"));
        }
    }
    Ok(permission)
}

/// Match each declared TOOL to a `--allow-exec` entry and resolve it to a real path.
fn resolve_tools(p: &Plan, options: &BuildOptions, warn: &Warn) -> Result<Vec<ResolvedTool>> {
    let mut allowed: Vec<(String, Option<String>)> = Vec::new();
    for spec in &options.allow_exec {
        let eq = spec.find('=');
        let name = match eq {
            Some(i) => &spec[..i],
            None => spec.as_str(),
        };
        if !is_identifier(name) || eq.is_some_and(|i| i == spec.len() - 1) {
            return Err(SitefileError::plain(E_USAGE, format!("--allow-exec expects NAME or NAME=/absolute/path (NAME is an identifier), got '{spec}'")));
        }
        let value = eq.map(|i| spec[i + 1..].to_string());
        match allowed.iter_mut().find(|(n, _)| n == name) {
            Some(slot) => slot.1 = value,
            None => allowed.push((name.to_string(), value)),
        }
    }
    let search_path = std::env::var("PATH").ok();
    let mut out = Vec::new();
    for t in &p.tools {
        let Some((_, explicit)) = allowed.iter().find(|(n, _)| *n == t.name) else {
            return Err(SitefileError::at(
                E_UNSUPPORTED,
                format!(
                    "TOOL {0} runs an external program, which must be allowed explicitly: add --allow-exec {0} (or --allow-exec {0}=/absolute/path)",
                    t.name
                ),
                &t.span,
            ));
        };
        let tool = resolve_tool(&t.name, explicit.as_deref(), search_path.as_deref(), Some(&t.span))?;
        if let Some(n) = &options.on_notice {
            n(&format!("tool {} -> {}", t.name, tool.path.display()));
        }
        out.push(tool);
    }
    for (name, _) in &allowed {
        if !p.tools.iter().any(|t| &t.name == name) {
            warn(&format!("--allow-exec {name} does not match any TOOL and was ignored"));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- operations

/// Variables visible to interpolation and templates.
#[derive(Clone, Copy)]
struct Scope<'a> {
    vars: &'a Env,
    resources: &'a HashMap<String, Resource>,
    /// The current element of a FOR loop.
    each: Option<(&'a str, &'a Value)>,
}

impl<'a> Scope<'a> {
    fn with_item(self, name: &'a str, value: &'a Value) -> Scope<'a> {
        Scope { each: Some((name, value)), ..self }
    }

    fn lookup(&self, name: &str, p: &[String], span: &SourceSpan) -> Result<Scalar> {
        if let Some((n, v)) = self.each {
            if n == name {
                return resolve_json_path(v, p, name, span);
            }
        }
        if let Some(v) = self.vars.get(name) {
            if !p.is_empty() {
                return Err(SitefileError::at(E_TYPE, format!("'{name}' is an ARG/LET value; dotted access only works on JSON imports"), span));
            }
            return Ok(v.clone());
        }
        let Some(res) = self.resources.get(name) else {
            return Err(SitefileError::at(E_UNDEFINED, format!("'{name}' is not an ARG, LET or IMPORT"), span));
        };
        let Resource::Value { format, value, .. } = res else {
            return Err(SitefileError::at(
                E_TYPE,
                format!("import '{name}' is a {}; templates can only read value imports (text, json, yaml, csv, markdown, wbn)", res.kind()),
                span,
            ));
        };
        if *format != ValueFormat::Text {
            return resolve_json_path(value, p, name, span);
        }
        if !p.is_empty() {
            return Err(SitefileError::at(
                E_TYPE,
                format!("import '{name}' is text; dotted access only works on structured imports (json, yaml, csv, markdown, wbn)"),
                span,
            ));
        }
        Ok(Scalar::Str(value.as_str().unwrap_or_default().to_string()))
    }

    /// Any value (lists, documents, scalars) for template blocks and FOR. `loose`: missing → None.
    fn value_of(&self, name: &str, p: &[String], span: &SourceSpan, loose: bool) -> Result<Option<Cow<'a, Value>>> {
        if let Some((n, v)) = self.each {
            if n == name {
                return Ok(walk(v, p, name, span, loose)?.map(Cow::Borrowed));
            }
        }
        if let Some(v) = self.vars.get(name) {
            let j = v.to_json();
            return Ok(walk(&j, p, name, span, loose)?.map(|x| Cow::Owned(x.clone())));
        }
        let Some(res) = self.resources.get(name) else {
            if loose {
                return Ok(None);
            }
            return Err(SitefileError::at(E_UNDEFINED, format!("'{name}' is not an ARG, LET or IMPORT"), span));
        };
        let Resource::Value { value, .. } = res else {
            return Err(SitefileError::at(
                E_TYPE,
                format!("import '{name}' is a {}; templates can only read value imports (text, json, yaml, csv, markdown, wbn)", res.kind()),
                span,
            ));
        };
        Ok(walk(value, p, name, span, loose)?.map(Cow::Borrowed))
    }

    /// Interpolate an operand with ARG/LET values and, inside FOR, the loop element.
    fn interpolate(&self, op: &Operand) -> Result<String> {
        let mut out = String::new();
        for part in &op.parts {
            match part {
                TemplatePart::Text(t) => out.push_str(t),
                TemplatePart::Var { name, path, span } => out.push_str(&self.lookup(name, path, span)?.to_js_string()),
            }
        }
        Ok(out)
    }
}

enum Source {
    File {
        bytes: Arc<[u8]>,
        mode: u32,
        basename: String,
        label: String,
        /// Heredoc bodies: the Sitefile label and the body's first line, for template error positions.
        inline: Option<(String, u32)>,
    },
    Tree {
        tree: Tree,
        label: String,
    },
}

impl Source {
    fn label(&self) -> &str {
        match self {
            Source::File { label, .. } | Source::Tree { label, .. } => label,
        }
    }
}

struct Exec<'a> {
    local: &'a LocalContext,
    resources: &'a HashMap<String, Resource>,
    /// Final trees of the stages finished so far, by name.
    stages: HashMap<String, Tree>,
    vars: &'a Env,
    label: &'a str,
    warn: Warn,
    plugins: PluginHost,
    tools: HashMap<String, ResolvedTool>,
    tool_policy: ToolPolicy,
    cache: Option<TransformCache>,
    /// Digest of each plugin's code (cache key); None when the folder was too large to hash.
    plugin_digests: HashMap<String, Option<String>>,
    stats: TransformStats,
}

fn unknown_stage(name: &str, span: &SourceSpan) -> SitefileError {
    SitefileError::at(E_UNDEFINED, format!("unknown stage '{name}'"), span)
}

fn policy_of(overwrite: bool) -> ConflictPolicy {
    if overwrite {
        ConflictPolicy::Overwrite
    } else {
        ConflictPolicy::Error
    }
}

impl<'a> Exec<'a> {
    fn scope(&self) -> Scope<'a> {
        Scope { vars: self.vars, resources: self.resources, each: None }
    }

    /// The tree a stage starts from.
    fn start_stage(&self, stage: &Stage) -> Result<Tree> {
        match &stage.base {
            StageBase::Scratch => Ok(Tree::empty()),
            StageBase::Stage { name } => self.stages.get(name).cloned().ok_or_else(|| unknown_stage(name, &stage.span)),
            StageBase::Local { path } => {
                let display = if path.is_empty() { "./".to_string() } else { format!("./{path}") };
                if self.local.stat(path, Some(&stage.span))? != EntryKind::Directory {
                    return Err(SitefileError::at(E_TYPE, format!("FROM '{display}' is a file; FROM needs a directory"), &stage.span));
                }
                match self.local.read_tree(path, &format!("{}:{} FROM", self.label, stage.span.line), Some(&stage.span))? {
                    Resource::Tree { tree, .. } => Ok(tree),
                    _ => unreachable!("read_tree returns a tree"),
                }
            }
        }
    }

    fn read_source(&self, r: &SourceRef, origin: &str) -> Result<Source> {
        match r {
            SourceRef::Inline { text, delimiter, body_line, .. } => Ok(Source::File {
                bytes: Arc::from(text.as_bytes()),
                mode: 0o644,
                basename: String::new(),
                label: format!("<<{delimiter}"),
                inline: Some((self.label.to_string(), *body_line)),
            }),
            SourceRef::Local { path, span } => {
                let label = format!("./{path}");
                if self.local.stat(path, Some(span))? == EntryKind::Directory {
                    let Resource::Tree { tree, .. } = self.local.read_tree(path, origin, Some(span))? else { unreachable!() };
                    return Ok(Source::Tree { tree, label });
                }
                let Resource::File { bytes, mode, basename, .. } = self.local.read_file(path, Some(span))? else { unreachable!() };
                Ok(Source::File { bytes: Arc::from(bytes), mode, basename, label, inline: None })
            }
            SourceRef::Stage { name, path, span } => {
                let tree = self.stages.get(name).ok_or_else(|| unknown_stage(name, span))?;
                let label = format!("--from={name} {path}");
                if let Some(node) = tree.get(path) {
                    return Ok(Source::File { bytes: node.bytes.clone(), mode: node.mode, basename: basename_virtual(path).to_string(), label, inline: None });
                }
                if tree.is_directory(path) {
                    return Ok(Source::Tree { tree: tree.subtree(path), label });
                }
                Err(SitefileError::at(E_NOT_FOUND, format!("'{path}' does not exist in stage '{name}'"), span))
            }
            SourceRef::Import { name, path, span } => {
                let label = if path == "/" { format!("{name}:") } else { format!("{name}:{path}") };
                let res = self.resources.get(name).ok_or_else(|| SitefileError::at(E_UNDEFINED, format!("unknown import '{name}'"), span))?;
                match res {
                    Resource::Value { format, .. } => Err(SitefileError::at(
                        E_TYPE,
                        format!(
                            "import '{name}' is a {} value: RENDER templates can read it, but it cannot be copied (import it AS file to copy it)",
                            format.as_str()
                        ),
                        span,
                    )),
                    Resource::File { bytes, mode, basename, .. } => {
                        if path != "/" {
                            return Err(SitefileError::at(E_PATH, format!("import '{name}' is a single file; use '{name}:'"), span));
                        }
                        Ok(Source::File { bytes: Arc::from(bytes.as_slice()), mode: *mode, basename: basename.clone(), label, inline: None })
                    }
                    Resource::Tree { tree, .. } => {
                        if let Some(node) = tree.get(path) {
                            return Ok(Source::File { bytes: node.bytes.clone(), mode: node.mode, basename: basename_virtual(path).to_string(), label, inline: None });
                        }
                        if tree.is_directory(path) {
                            return Ok(Source::Tree { tree: tree.subtree(path), label });
                        }
                        Err(SitefileError::at(E_NOT_FOUND, format!("'{path}' does not exist in import '{name}'"), span))
                    }
                }
            }
        }
    }

    fn execute(&mut self, op: &Operation, tree: Tree) -> Result<Tree> {
        let origin = format!("{}:{} {}", self.label, op.span().line, op.keyword());
        match op {
            Operation::Transform { .. } => self.execute_transform(op, tree),
            Operation::Copy { source, dest, dest_is_dir, overwrite, span } => {
                let policy = policy_of(*overwrite);
                match self.read_source(source, &origin)? {
                    Source::File { bytes, mode, basename, label, inline } => {
                        let into_dir = *dest_is_dir || tree.is_directory(dest);
                        if into_dir && inline.is_some() {
                            return Err(SitefileError::at(E_PATH, format!("'{dest}' is a directory; an inline {label} body needs a file destination"), span));
                        }
                        let target = if into_dir { join_virtual(dest, &basename) } else { dest.clone() };
                        tree.with_files([(target, make_file(bytes, origin, mode))], policy, Some(span))
                    }
                    Source::Tree { tree: src, label } => {
                        if src.is_empty() {
                            (self.warn)(&format!("{origin}: '{label}' contains no files"));
                        }
                        let files: Vec<(String, Node)> = src
                            .mounted_at(dest)
                            .into_iter()
                            .map(|(p, n)| (p, Arc::new(FileNode { bytes: n.bytes.clone(), mode: n.mode, digest: n.digest.clone(), origin: origin.clone() })))
                            .collect();
                        tree.with_files(files, policy, Some(span))
                    }
                }
            }
            Operation::Render { template, dest, each, escape, overwrite, span } => {
                let policy = policy_of(*overwrite);
                let src = self.read_source(template, &origin)?;
                let Source::File { bytes, inline, label, .. } = src else {
                    return Err(SitefileError::at(E_TYPE, format!("RENDER template '{}' is a directory, not a file", src.label()), template.span()));
                };
                let text = decode_utf8(&bytes, &format!("template '{label}'"), Some(template.span()))?;
                let parts = match &inline {
                    Some((file, first)) => parse_render_template(&text, file, *first)?,
                    None => parse_render_template(&text, &label, 1)?,
                };
                let scope = self.scope();
                let Some(each) = each else {
                    let out = render_template(
                        &parts,
                        &|n: &str, p: &[String], s: &SourceSpan| scope.lookup(n, p, s),
                        *escape,
                        Some(&|n: &str, p: &[String], s: &SourceSpan, l: bool| scope.value_of(n, p, s, l)),
                    )?;
                    return tree.with_files([(dest.clone(), make_file(out.into_bytes(), origin, 0o644))], policy, Some(span));
                };

                // RENDER … FOR item IN list: one file per element, the destination interpolated per element.
                let (as_name, list, dest_op) = (&each.as_name, &each.list, &each.dest);
                let items = scope.value_of(list, &[], span, false)?;
                let Some(Value::Array(items)) = items.as_deref() else {
                    return Err(SitefileError::at(
                        E_TYPE,
                        format!("FOR {as_name} IN {list}: '{list}' is not a list (import a directory AS markdown, or a JSON array)"),
                        span,
                    ));
                };
                if items.is_empty() {
                    (self.warn)(&format!("{origin}: FOR {as_name} IN {list} renders nothing: '{list}' is empty"));
                }
                let mut files: Vec<(String, Node)> = Vec::new();
                for item in items {
                    let mut raw = String::new();
                    for part in &dest_op.parts {
                        match part {
                            TemplatePart::Text(t) => raw.push_str(t),
                            TemplatePart::Var { name, path, span } => {
                                let v = if name == as_name { resolve_json_path(item, path, as_name, span)? } else { scope.lookup(name, path, span)? };
                                raw.push_str(&v.to_js_string());
                            }
                        }
                    }
                    let target = normalize_virtual(&raw, Some(&dest_op.span), &format!("RENDER destination for {as_name}"))?;
                    if target.trailing_slash {
                        return Err(SitefileError::at(E_PATH, format!("RENDER destination '{raw}' must be a file path, not a directory"), &dest_op.span));
                    }
                    let item_scope = scope.with_item(as_name, item);
                    let out = render_template(
                        &parts,
                        &|n: &str, p: &[String], s: &SourceSpan| item_scope.lookup(n, p, s),
                        *escape,
                        Some(&|n: &str, p: &[String], s: &SourceSpan, l: bool| item_scope.value_of(n, p, s, l)),
                    )?;
                    files.push((target.path, make_file(out.into_bytes(), origin.clone(), 0o644)));
                }
                tree.with_files(files, policy, Some(span))
            }
        }
    }

    fn execute_transform(&mut self, op: &Operation, tree: Tree) -> Result<Tree> {
        let Operation::Transform { source, plugin, dest, options, each, args, overwrite, span } = op else { unreachable!() };
        let origin = format!("{}:{} TRANSFORM", self.label, span.line);
        let policy = policy_of(*overwrite);
        let scope = self.scope();
        let list_value;
        let items: Vec<Option<&Value>> = match each {
            Some(e) => {
                list_value = scope.value_of(&e.list, &[], span, false)?;
                let Some(Value::Array(list)) = list_value.as_deref() else {
                    return Err(SitefileError::at(E_TYPE, format!("FOR {} IN {}: '{}' is not a list", e.as_name, e.list, e.list), span));
                };
                if list.is_empty() {
                    (self.warn)(&format!("{origin}: FOR {} IN {} produces nothing: the list is empty", e.as_name, e.list));
                }
                list.iter().map(Some).collect()
            }
            None => vec![None],
        };

        // The source is the same for every element; read it once.
        let mut input: Option<PluginInput> = None;
        let mut data: Option<Value> = None;
        if let Some(src_ref) = source {
            let value_import = match src_ref {
                SourceRef::Import { name, path, span } => match self.resources.get(name) {
                    Some(Resource::Value { value, .. }) => {
                        if path != "/" {
                            return Err(SitefileError::at(E_PATH, format!("import '{name}' is a value; use '{name}:'"), span));
                        }
                        Some(value.clone())
                    }
                    _ => None,
                },
                _ => None,
            };
            if let Some(v) = value_import {
                data = Some(v);
            } else {
                let src = self.read_source(src_ref, &origin)?;
                let Source::File { bytes, basename, label, .. } = src else {
                    return Err(SitefileError::at(
                        E_TYPE,
                        format!("TRANSFORM source '{}' is a directory; plugins take one file", src.label()),
                        src_ref.span(),
                    ));
                };
                let name = if basename.is_empty() { label.clone() } else { basename };
                input = Some(PluginInput::new(label, name, bytes.to_vec()));
            }
        }

        let tool = self.tools.get(plugin).cloned();
        if let (Some(_), None, Some(s)) = (&tool, &input, source) {
            return Err(SitefileError::at(E_TYPE, format!("TRANSFORM … WITH {plugin}: a TOOL needs a file source, not a value import"), s.span()));
        }
        let input_digest = input.as_ref().map(|i| sha256(&i.bytes));
        let mut plugin_digest: Option<String> = None;
        if self.cache.is_some() && tool.is_none() {
            if !self.plugin_digests.contains_key(plugin) {
                // A wasm plugin is just its module; a JS plugin can read its whole folder.
                let digest = match self.plugins.wasm_bytes(plugin) {
                    Some(w) => Some(sha256(w)),
                    None => match self.plugins.folder_of(plugin) {
                        Some(dir) => digest_folder(&dir).ok().flatten(),
                        None => None,
                    },
                };
                if digest.is_none() {
                    (self.warn)(&format!("{origin}: PLUGIN {plugin}'s folder is too large to hash; its results are not cached"));
                }
                self.plugin_digests.insert(plugin.clone(), digest);
            }
            plugin_digest = self.plugin_digests.get(plugin).cloned().flatten();
        }

        let mut files: Vec<(String, Node)> = Vec::new();
        for item in items {
            let item_scope = match (each, item) {
                (Some(e), Some(v)) => scope.with_item(&e.as_name, v),
                _ => scope,
            };
            let raw_dest = item_scope.interpolate(dest)?;
            let d = normalize_virtual(&raw_dest, Some(&dest.span), "TRANSFORM destination")?;
            // Object.create(null) + assignment: a repeated key keeps its first position, the last value.
            let mut opts: Vec<(String, String)> = Vec::new();
            for o in options {
                let v = item_scope.interpolate(&o.value)?;
                match opts.iter_mut().find(|(k, _)| *k == o.key) {
                    Some(slot) => slot.1 = v,
                    None => opts.push((o.key.clone(), v)),
                }
            }
            let request = PluginRequest {
                dest: d.path.clone(),
                options: opts.clone(),
                input: input.clone(),
                data: data.clone(),
                item: item.cloned(),
            };
            let mut argv: Option<Vec<String>> = None;
            if tool.is_some() {
                if d.trailing_slash {
                    return Err(SitefileError::at(E_PLUGIN, format!("TOOL {plugin} writes one file, but '{raw_dest}' is a directory destination"), &dest.span));
                }
                let mut v = Vec::new();
                for a in args.as_deref().unwrap_or(&[]) {
                    v.push(item_scope.interpolate(a)?);
                }
                argv = Some(v);
            }
            // Everything the result can depend on: the code (plugin digest, or tool binary + argv) and the request.
            let cacheable = self.cache.is_some() && (tool.is_some() || plugin_digest.is_some());
            let key = if cacheable {
                let code = match &tool {
                    Some(t) => json!({
                        "tool": { "path": t.path.to_string_lossy(), "size": t.size, "mtimeMs": t.mtime_ms },
                        "args": argv.clone().unwrap_or_default(),
                    }),
                    None => json!({ "plugin": plugin_digest }),
                };
                let mut req = Map::new();
                req.insert("dest".into(), Value::String(d.path.clone()));
                let mut om = Map::new();
                for (k, v) in &opts {
                    om.insert(k.clone(), Value::String(v.clone()));
                }
                req.insert("options".into(), Value::Object(om));
                if let (Some(i), Some(dg)) = (&input, &input_digest) {
                    req.insert("input".into(), json!({ "path": i.path, "name": i.name, "digest": dg }));
                }
                if let Some(dt) = &data {
                    req.insert("data".into(), dt.clone());
                }
                if let Some(it) = item {
                    req.insert("item".into(), it.clone());
                }
                Some(cache_key(&json!({ "code": code, "request": Value::Object(req) })))
            } else {
                None
            };
            let mut result: Option<PluginResult> = match (&key, self.cache.as_mut()) {
                (Some(k), Some(c)) => c.get(k),
                _ => None,
            };
            if result.is_none() {
                let r = if let Some(t) = &tool {
                    self.stats.tool_runs += 1;
                    let call = ToolCall {
                        args: argv.clone().unwrap_or_default(),
                        dest: d.path.clone(),
                        options: opts.clone(),
                        input: input.as_ref().map(|i| ToolInput { name: i.name.clone(), bytes: i.bytes.clone() }),
                    };
                    let warn = self.warn.clone();
                    run_tool(t, &call, &self.tool_policy, &move |l: &str| warn(l), Some(span))?
                } else {
                    self.plugins.call(plugin, &request, Some(span))?
                };
                if let Some(k) = &key {
                    if tool.is_some() || self.plugins.cacheable(plugin) {
                        if let Some(c) = self.cache.as_mut() {
                            c.put(k, &r);
                        }
                    }
                }
                result = Some(r);
            }
            match result.expect("set above") {
                PluginResult::File(bytes) => {
                    if d.trailing_slash {
                        return Err(SitefileError::at(
                            E_PLUGIN,
                            format!("PLUGIN {plugin} returned one file, but '{raw_dest}' is a directory destination"),
                            &dest.span,
                        ));
                    }
                    files.push((d.path.clone(), make_file(bytes, origin.clone(), 0o644)));
                }
                PluginResult::Files(list) => {
                    // { files }: placed under the destination directory.
                    for (rel, bytes) in list {
                        let base = if d.path == "/" { "" } else { d.path.as_str() };
                        let p = normalize_virtual(&format!("{base}/{rel}"), Some(&dest.span), &format!("file '{rel}' returned by PLUGIN {plugin}"))?.path;
                        if rel.contains("..") || rel.starts_with('/') {
                            return Err(SitefileError::at(E_PLUGIN, format!("PLUGIN {plugin} returned an unsafe path '{rel}'"), span));
                        }
                        files.push((p, make_file(bytes, origin.clone(), 0o644)));
                    }
                }
            }
        }
        tree.with_files(files, policy, Some(span))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_and_join() {
        assert_eq!(relative_path(Path::new("/a/b"), Path::new("/a/b/c/d")), "c/d");
        assert_eq!(relative_path(Path::new("/a/b"), Path::new("/a/x")), "../x");
        assert_eq!(relative_path(Path::new("/a"), Path::new("/a")), "");
        assert_eq!(join_path(".", "dist"), "dist");
        assert_eq!(join_path("examples/hello/", "dist"), "examples/hello/dist");
        assert_eq!(join_path("/x/../y", "Sitefile"), "/y/Sitefile");
    }

    #[test]
    fn url_basenames() {
        assert_eq!(url_basename("https://h.example/a/b.json?x=1"), "b.json");
        assert_eq!(url_basename("https://h.example/dir/"), "dir");
        assert_eq!(url_basename("https://h.example"), "");
    }

    #[test]
    fn map_limit_keeps_order_and_first_error() {
        let items: Vec<u32> = (0..20).collect();
        let r = map_limit(&items, 4, |i| Ok(i * 2)).unwrap();
        assert_eq!(r, (0..20).map(|i| i * 2).collect::<Vec<_>>());
        let e = map_limit(&items, 4, |i| if *i == 3 || *i == 7 { Err(SitefileError::plain(E_FETCH, format!("{i}"))) } else { Ok(*i) }).unwrap_err();
        assert_eq!(e.detail, "3");
    }
}

//! Run a build (port of `src/executor.ts`): read the Sitefile, resolve the
//! imports, execute every stage's operations and export the final tree.
//!
//! Messages and warnings match the TypeScript implementation word for word;
//! `../conformance` checks it.

use crate::base_image::{base_ref, base_ref_as, read_layer_blobs, resolve_base, resolve_from_image, FromImage, ResolveBaseOptions};
use crate::build_args::BuildArgs;
use crate::registry::RegistryOptions;
use crate::store::Store;
use crate::digest::sha256;
use crate::documents::{data_collection, markdown_collection, sort_documents, DataKind, MarkdownDocument};
use crate::memo::DocMemo;
use crate::error::{ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::export::archive_writers::{source_date_epoch, WriterOptions};
use crate::export::exporter::{export_directory, export_file, is_exporter_temp_name, resolve_path, ExportOptions};
use crate::export::formats::{encode_tree, is_archive_format, EncodeOptions};
use crate::export::oci::{build_oci_image, image_meta, layout_tree, oci_archive, BaseImage, OciOptions};
use crate::language::ast::{ExportFormat, FEED_ITEM, ImportKind, OciCompression, Operand, Program, TemplatePart};
use crate::language::validate::parse_program;
use crate::paths::{basename_virtual, join_virtual, normalize_virtual};
use crate::planner::{plan, target_plan, ImportLocation, ImportSpec, Operation, Plan, PluginSpec, SourceRef, Stage, StageBase};
use crate::plugins::cache::{cache_key, digest_folder, CacheStats, TransformCache};
use crate::plugins::exec::{resolve_tool, run_tool, ResolvedTool, ToolCall, ToolInput, ToolPolicy, DEFAULT_TOOL_POLICY};
use crate::plugins::host::{PluginHost, PluginInput, PluginPolicy, PluginRequest, PluginResult, DEFAULT_PLUGIN_POLICY};
use crate::plugins::wasm::check_wasm_plugin;
use crate::profile::{begin, Profiler};
use crate::memo::{lock as lock_memo, memo_key, memoize, BuildMemo, MemoStats, SharedMemo};
use crate::render::{decode_utf8, parse_render_template, render_template, resolve_json_path, walk, TemplateNode};
use crate::resources::convert::{bytes_to_resource, verify_integrity, BytesInfo};
use crate::resources::https::fetch_remote;
use crate::resources::local::{EntryKind, LocalContext, LocalContextOptions};
use crate::resources::lockfile::{LockEntry, Lockfile};
use crate::resources::vendor::{find_vendor_dir, Vendor};
use crate::resources::types::{
    ArchiveLimits, FetchPolicy, Resource, ValueFormat, DEFAULT_ARCHIVE_LIMITS, DEFAULT_FETCH_POLICY,
};
use crate::site::{atom_xml, headers_file, page_url, parse_feed_date, redirects_file, sitemap_xml, FeedEntry};
use crate::tree::{make_file, ConflictPolicy, FileNode, Node, Tree};
use crate::values::{js_string_of, Env, Scalar};
use serde_json::{json, Map, Value};
use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};
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
    /// `--target <stage>`: build up to this named stage and export its whole tree (planner `target_plan`).
    pub target: Option<String>,
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
    /// Context-relative vendor directory written by `sitefile vendor` (default `vendor/sitefile`, used
    /// when its manifest.json exists; a directory given here must exist). Remote IMPORTs and BASE blobs
    /// found there are read from it instead of the network, verified as usual. It is never part of a
    /// directory copy.
    pub vendor_dir: Option<String>,
    /// `--strict-pins`: fail (E_UNPINNED) before fetching anything when a remote IMPORT has neither
    /// INTEGRITY nor a lockfile entry, or the BASE image is neither `@sha256:`-pinned nor locked.
    pub strict_pins: bool,
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
    /// Consulted on a local cache miss and told about every entry used (`--cache-from` / `--cache-to`). Needs `cache_dir`.
    pub remote_cache: Option<Arc<dyn crate::plugins::cache::RemoteCache>>,
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
    /// `--profile`: record a timing trace of the build here (src/profile.rs).
    pub profiler: Option<Arc<Profiler>>,
    /// Other paths this command writes (the --profile file): skipped when walking the context, like `out_dir`.
    pub also_writes: Vec<String>,
    /// Incremental rebuilds: reuse the results of steps whose inputs did not change since an
    /// earlier build with the same memo (watch mode keeps one per session; src/memo.rs).
    pub memo: Option<SharedMemo>,
    /// Keep every intermediate tree in [`BuildResult::steps`] (for `sitefile why`).
    pub record_steps: bool,
}

impl Default for BuildOptions {
    fn default() -> Self {
        BuildOptions {
            context_dir: ".".into(),
            sitefile: None,
            build_args: BuildArgs::new(),
            target: None,
            out_dir: None,
            fetch_policy: DEFAULT_FETCH_POLICY,
            archive_limits: DEFAULT_ARCHIVE_LIMITS,
            lockfile: LockfileSetting::Default,
            frozen_lockfile: false,
            update_lock: false,
            vendor_dir: None,
            strict_pins: false,
            concurrency: 4,
            plugin_policy: DEFAULT_PLUGIN_POLICY,
            allow_hardlinks: false,
            allow_exec: Vec::new(),
            allow_plugin: Vec::new(),
            allow_all_plugins: false,
            tool_policy: DEFAULT_TOOL_POLICY,
            cache_dir: None,
            remote_cache: None,
            on_warning: None,
            on_notice: None,
            platform: None,
            base_image: None,
            skip_base: false,
            insecure_registries: Vec::new(),
            profiler: None,
            also_writes: Vec::new(),
            memo: None,
            record_steps: false,
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
    /// With `memo`: how many steps were reused and how many ran.
    pub memo_stats: Option<MemoStats>,
    /// With `record_steps`: per stage of `plan.stages`, the tree it starts from, then the tree after each of its
    /// operations (`steps[s][i + 1]` follows `operations[i]`). `tree` is the last one plus the EXPORT site files.
    pub steps: Option<Vec<Vec<Tree>>>,
    /// `--strict-pins`: how each remote input was pinned (recorded in the provenance).
    pub pins: Option<Pins>,
    /// The `FROM image` stage bases this build read, in stage order.
    pub images: Vec<FromImageUse>,
}

/// A `FROM image` base a build used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FromImageUse {
    /// `registry/name:tag`, or the local name as written.
    pub name: String,
    pub digest: String,
    /// The FROM line.
    pub line: u32,
    pub local: bool,
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
    plan_for(&loaded.program, options)
}

/// The plan of a program for these options (`target` applied).
fn plan_for(program: &Program, options: &BuildOptions) -> Result<Plan> {
    let p = plan(program, &options.build_args)?;
    match &options.target {
        Some(t) => target_plan(p, t),
        None => Ok(p),
    }
}

type Warn = Callback;

/// realpath of the nearest existing ancestor, with the missing tail re-joined.
pub(crate) fn realpath_loose(p: &Path) -> PathBuf {
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

/// The lockfile a build would use: `Sitefile.lock` next to the Sitefile, unless disabled.
fn lock_path_for(setting: &LockfileSetting, sitefile_path: &Path) -> Option<PathBuf> {
    match setting {
        LockfileSetting::Disabled => None,
        LockfileSetting::Path(p) => Some(p.clone()),
        LockfileSetting::Default => Some(sitefile_path.parent().unwrap_or(Path::new("/")).join("Sitefile.lock")),
    }
}

/// The BASE image a build resolves: `--base`, else `EXPORT ... AS oci BASE`, unless skipped.
fn base_ref_of(options: &BuildOptions, p: &Plan) -> Option<String> {
    if options.skip_base {
        return None;
    }
    options.base_image.clone().or_else(|| if p.export_format == ExportFormat::Oci { p.export_base.clone() } else { None })
}

/// How the remote inputs of a `--strict-pins` build are pinned: remote IMPORTs by name
/// (`integrity` or `lockfile`), and the BASE (`digest` or `lockfile`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pins {
    pub imports: Vec<(String, &'static str)>,
    pub base: Option<&'static str>,
    /// `FROM image` bases with a registry, by FROM line (`digest` or `lockfile`).
    pub images: Vec<(u32, &'static str)>,
}

/// The Sitefile.lock `images` key of a registry `FROM image` reference.
pub fn image_lock_key(reference: &str, lead: &str, span: &SourceSpan) -> Result<String> {
    Ok(base_ref_as(reference, Some(span), &format!("{lead} image"))?.lock_key)
}

impl Pins {
    pub fn import(&self, name: &str) -> Option<&'static str> {
        self.imports.iter().find(|(n, _)| n == name).map(|(_, b)| *b)
    }
}

/// `--strict-pins`: every remote input must be pinned, so the build is reproducible. A remote IMPORT
/// needs INTEGRITY or a `Sitefile.lock` entry; a BASE image needs `@sha256:` or a lockfile `images`
/// entry. `lock` is `None` with --no-lockfile (only the Sitefile's own pins count then). Checked in
/// source order, before anything is fetched; the first unpinned input fails with E_UNPINNED.
pub fn check_pins(p: &Plan, lock: Option<&Lockfile>, base: Option<&str>) -> Result<Pins> {
    let mut pins = Pins::default();
    for spec in &p.imports {
        let ImportLocation::Remote { url } = &spec.location else { continue };
        if spec.integrity.is_some() {
            pins.imports.push((spec.name.clone(), "integrity"));
            continue;
        }
        if lock.is_some_and(|l| l.get(url).is_some()) {
            pins.imports.push((spec.name.clone(), "lockfile"));
            continue;
        }
        return Err(SitefileError::at(
            E_UNPINNED,
            format!(
                "IMPORT {}: '{url}' is not pinned (--strict-pins); add INTEGRITY sha256:<hex>, or record it in Sitefile.lock with a build without --strict-pins",
                spec.name
            ),
            &spec.span,
        ));
    }
    for st in &p.stages {
        let StageBase::Image { reference, local: false, lead, span, .. } = &st.base else { continue };
        let b = base_ref_as(reference, Some(span), &format!("{lead} image"))?;
        if b.r.digest.is_some() {
            pins.images.push((st.span.line, "digest"));
        } else if lock.is_some_and(|l| l.get_image(&b.lock_key).is_some()) {
            pins.images.push((st.span.line, "lockfile"));
        } else {
            return Err(SitefileError::at(
                E_UNPINNED,
                format!(
                    "{lead} image {reference}: '{}' is not pinned (--strict-pins); pin it with @sha256:..., or record it in Sitefile.lock with a build without --strict-pins",
                    b.lock_key
                ),
                span,
            ));
        }
    }
    let Some(input) = base else { return Ok(pins) };
    let b = base_ref(input, None)?;
    if b.r.digest.is_some() {
        pins.base = Some("digest");
        return Ok(pins);
    }
    if lock.is_some_and(|l| l.get_image(&b.lock_key).is_some()) {
        pins.base = Some("lockfile");
        return Ok(pins);
    }
    Err(SitefileError::plain(
        E_UNPINNED,
        format!(
            "BASE {input}: '{}' is not pinned (--strict-pins); pin it with @sha256:..., or record it in Sitefile.lock with a build without --strict-pins",
            b.lock_key
        ),
    ))
}

/// `validate --strict-pins`: check the pins of `p` against the lockfile without building or fetching.
pub fn check_plan_pins(options: &BuildOptions, p: &Plan) -> Result<()> {
    let sitefile_path = resolve_path(Path::new(&options.sitefile.clone().unwrap_or_else(|| join_path(&options.context_dir, "Sitefile"))));
    let lock = match lock_path_for(&options.lockfile, &sitefile_path) {
        Some(path) => Some(Lockfile::load(path)?),
        None => None,
    };
    check_pins(p, lock.as_ref(), base_ref_of(options, p).as_deref()).map(|_| ())
}

/// Run a build in memory.
pub fn build(options: &BuildOptions) -> Result<BuildResult> {
    let Some(memo) = &options.memo else { return build_once(options) };
    lock_memo(memo).begin();
    let r = build_once(options);
    let mut m = lock_memo(memo);
    m.finish(r.is_ok());
    let stats = m.stats;
    r.map(|r| BuildResult { memo_stats: Some(stats), ..r })
}

fn build_once(options: &BuildOptions) -> Result<BuildResult> {
    let prof = options.profiler.as_deref();
    let parse_span = begin(prof, || "parse".into(), "build", Vec::new);
    let Loaded { program, sitefile_path, label } = load_program(&options.context_dir, options.sitefile.as_deref())?;
    let full = plan(&program, &options.build_args)?;
    let all_stages = full.stages.clone();
    let p = match &options.target {
        Some(t) => target_plan(full, t)?,
        None => full,
    };
    let mut parse_args = vec![("imports", json!(p.imports.len())), ("stages", json!(p.stages.len()))];
    if let Some(t) = &p.target {
        parse_args.push(("target", json!(t)));
    }
    parse_span.end(parse_args);
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
    let vendor_dir = find_vendor_dir(&context_real, options.vendor_dir.as_deref())?;
    if let Some(d) = &vendor_dir {
        ignore.push(d.clone());
    }
    for w in &options.also_writes {
        let rel = relative_path(&context_real, &realpath_loose(&resolve_path(Path::new(w))));
        if !rel.is_empty() && !rel.starts_with("..") && !rel.starts_with('/') {
            ignore.push(rel);
        }
    }
    let local = LocalContext::open(
        &context_real,
        &LocalContextOptions { ignore: Some(ignore), sitefileignore: true, allow_hardlinks: options.allow_hardlinks },
    )?;
    let vendor = match &vendor_dir {
        Some(d) => Some(Vendor::open(&local, d)?),
        None => None,
    };
    let lock = match lock_path_for(&options.lockfile, &sitefile_path) {
        Some(p) => Some(Lockfile::load(p)?),
        None => None,
    };
    // BASE: the image to build on (resolved after the files, below).
    let base_ref = base_ref_of(options, &p);
    let pins = if options.strict_pins { Some(check_pins(&p, lock.as_ref(), base_ref.as_deref())?) } else { None };
    let mut lock = lock;
    // A --target build skips later stages; their FROM image lockfile entries are kept all the same.
    for st in &all_stages[p.stages.len()..] {
        if let (StageBase::Image { reference, local: false, lead, span, .. }, Some(l)) = (&st.base, lock.as_mut()) {
            l.mark_image_used(&image_lock_key(reference, lead, span)?);
        }
    }
    let lock = Mutex::new(lock);

    let env = ImportEnv {
        local: &local,
        archive_limits: &options.archive_limits,
        remote: RemoteEnv {
            policy: &options.fetch_policy,
            lock: &lock,
            label: &label,
            warn: &warn,
            frozen: options.frozen_lockfile,
            update: options.update_lock,
            vendor: vendor.as_ref().map(|v| (v, &local)),
        },
        memo: options.memo.as_deref(),
    };
    // Imports resolve concurrently: each gets its own trace thread, reserved in Sitefile order.
    let import_spans: Vec<_> = p
        .imports
        .iter()
        .enumerate()
        .map(|(i, spec)| {
            prof.map(|pr| {
                let tid = i as u64 + 2;
                pr.name_thread(tid, format!("import {}", spec.name));
                let as_kind = spec.as_kind.map(|k| k.as_str()).unwrap_or("file");
                pr.reserve(format!("import {}", spec.name), "import", vec![("uri", json!(spec.uri)), ("as", json!(as_kind))], tid)
            })
        })
        .collect();
    let indexed: Vec<(usize, &ImportSpec)> = p.imports.iter().enumerate().collect();
    let resolved = map_limit(&indexed, options.concurrency.max(1), |(i, spec)| {
        if let Some(s) = &import_spans[*i] {
            s.start();
        }
        let r = resolve_import(spec, &env)?;
        if let Some(s) = &import_spans[*i] {
            s.end(vec![("kind", json!(r.0.kind()))]);
        }
        Ok(r)
    })?;
    let mut resources: HashMap<String, Resource> = HashMap::new();
    let mut resource_list: Vec<(String, Resource)> = Vec::new();
    // Incremental rebuilds: what each import is (its spec and the digest of what it read).
    let mut resource_keys: HashMap<String, String> = HashMap::new();
    for (spec, (res, key)) in p.imports.iter().zip(resolved) {
        let res = sorted(spec, res)?;
        resources.insert(spec.name.clone(), res.clone());
        resource_list.push((spec.name.clone(), res));
        resource_keys.insert(spec.name.clone(), key);
    }
    let memo = options.memo.as_deref();
    // The Sitefile label and its ARG/LET values (sorted: Env is a HashMap).
    let env_key = match memo {
        Some(_) => {
            let mut vars: Vec<_> = p.vars.iter().collect();
            vars.sort_by(|a, b| a.0.cmp(b.0));
            memo_key(&["env", &label, &format!("{vars:?}")])
        }
        None => String::new(),
    };

    // FROM image stage bases: resolved when their stage starts (registry pull or local store).
    let image_store: std::cell::OnceCell<Store> = std::cell::OnceCell::new();
    let used_images: Mutex<Vec<FromImageUse>> = Mutex::new(Vec::new());
    let image_warn = {
        let w = warn.clone();
        move |m: &str| w(m)
    };
    let from_image = |reference: &str, at: Option<&str>, is_local: bool, lead: &str, span: &SourceSpan| -> Result<FromImage> {
        let store = match image_store.get() {
            Some(s) => s,
            None => {
                let s = Store::open_default()?;
                image_store.get_or_init(|| s)
            }
        };
        let o = ResolveBaseOptions {
            platform: options.platform.clone(),
            frozen: options.frozen_lockfile,
            update: options.update_lock,
            vendor: vendor.as_ref().map(|v| (v, &local)),
            registry: RegistryOptions {
                insecure_registries: options.insecure_registries.clone(),
                allow_private_network: options.fetch_policy.allow_private_network,
                ..Default::default()
            },
            warn: Some(&image_warn),
            span: Some(span.clone()),
            lead: Some(format!("{lead} image")),
            ..ResolveBaseOptions::new(store)
        };
        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let r = resolve_from_image(reference, at, is_local, guard.as_mut(), &o)?;
        used_images.lock().unwrap_or_else(|e| e.into_inner()).push(FromImageUse {
            name: r.name.clone(),
            digest: r.digest.clone(),
            line: span.line,
            local: is_local,
        });
        Ok(r)
    };

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
        cache: options.cache_dir.as_ref().map(|d| TransformCache::new(d).with_remote(options.remote_cache.clone())),
        plugin_digests: HashMap::new(),
        stats: TransformStats::default(),
        profiler: prof,
        memo,
        resource_keys,
        env_key,
        stage_keys: HashMap::new(),
        recorded: None,
        unrecorded: warn.clone(),
        from_image: &from_image,
    };
    let mut steps: Option<Vec<Vec<Tree>>> = options.record_steps.then(Vec::new);
    let run = (|| -> Result<Tree> {
        for spec in &p.plugins {
            let s = begin(prof, || format!("PLUGIN {}", spec.name), "plugin", || vec![("path", json!(format!("./{}", spec.path)))]);
            register_plugin(spec, &local, &mut exec.plugins, &plugin_permission)?;
            s.end(Vec::new());
        }
        let mut tree = Tree::empty();
        for (i, stage) in p.stages.iter().enumerate() {
            let name = stage.name.clone().unwrap_or_else(|| format!("#{}", i + 1));
            let s = begin(prof, || format!("stage {name}"), "stage", || vec![("from", json!(stage_from(stage)))]);
            let (start, mut key) = exec.start_stage(stage)?;
            tree = start;
            let mut record = steps.as_ref().map(|_| vec![tree.clone()]);
            for op in &stage.operations {
                let line = op.span().line;
                let o = begin(prof, || format!("{label}:{line} {}", op.keyword()), "operation", || vec![("line", json!(line))]);
                let (out, k, state) = exec.execute_op(op, tree, &key)?;
                tree = out;
                key = k;
                if let Some(r) = record.as_mut() {
                    r.push(tree.clone());
                }
                let mut args = vec![("files", json!(tree.len()))];
                if let Some(state) = state {
                    args.push(("memo", json!(state)));
                }
                o.end(args);
            }
            if let (Some(all), Some(r)) = (steps.as_mut(), record) {
                all.push(r);
            }
            if let Some(name) = &stage.name {
                exec.stages.insert(name.clone(), tree.clone());
                exec.stage_keys.insert(name.clone(), key);
            }
            s.end(vec![("files", json!(tree.len()))]);
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
    let images = used_images.into_inner().unwrap_or_default();

    if tree.is_file(&p.export_path) {
        return Err(SitefileError::plain(E_EXPORT, format!("EXPORT path '{}' is a file; export a directory", p.export_path)));
    }
    let tree = with_site_rules(tree, &p, &label)?;
    let output = tree.subtree(&p.export_path);
    if output.is_empty() {
        warn(&match &p.target {
            None => format!("EXPORT {} contains no files", p.export_path),
            Some(t) => format!("--target {t}: stage '{t}' contains no files"),
        });
    }

    // BASE: the image to build on, resolved after the files so a failing build never pulls anything.
    let mut lock = lock.into_inner().unwrap_or(None);
    let base_span = match &base_ref {
        Some(r) => begin(prof, || format!("BASE {r}"), "import", Vec::new),
        None => crate::profile::Span::none(),
    };
    let base = match base_ref {
        Some(r) => {
            let store = Store::open_default()?;
            let w = warn.clone();
            let warn_fn = move |m: &str| w(m);
            let o = ResolveBaseOptions {
                platform: options.platform.clone(),
                frozen: options.frozen_lockfile,
                update: options.update_lock,
                vendor: vendor.as_ref().map(|v| (v, &local)),
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
    base_span.end(Vec::new());

    let lockfile_written = match lock.as_mut() {
        Some(l) if !options.frozen_lockfile => l.save()?,
        _ => false,
    };
    let warnings = warnings.lock().map(|w| w.clone()).unwrap_or_default();
    Ok(BuildResult {
        plan: p,
        tree,
        output,
        resources: resource_list,
        warnings,
        lockfile_written,
        base,
        tools,
        transform_stats: stats,
        steps,
        pins,
        memo_stats: None,
        images,
    })
}

/// REDIRECT and HEADER lines become `/_redirects` and `/_headers` at the root of the exported directory.
fn with_site_rules(mut tree: Tree, p: &Plan, label: &str) -> Result<Tree> {
    let redirects = redirects_file(p.redirects.iter().map(|r| (r.from.as_str(), r.to.as_str(), r.status)));
    let headers = headers_file(p.headers.iter().map(|h| (h.pattern.as_str(), h.name.as_str(), h.value.as_str())));
    /// (file name, content, each rule's `--overwrite` and span, instruction)
    type SiteFile<'a> = (&'a str, String, Vec<(bool, &'a SourceSpan)>, &'a str);
    let files: [SiteFile; 2] = [
        ("_redirects", redirects, p.redirects.iter().map(|r| (r.overwrite, &r.span)).collect(), "REDIRECT"),
        ("_headers", headers, p.headers.iter().map(|h| (h.overwrite, &h.span)).collect(), "HEADER"),
    ];
    for (name, text, rules, kind) in files {
        let Some((_, first)) = rules.first() else { continue };
        let policy = policy_of(rules.iter().any(|(o, _)| *o));
        let node = make_file(text.into_bytes(), format!("{label}:{} {kind}", first.line), 0o644);
        tree = tree.with_files([(join_virtual(&p.export_path, name), node)], policy, Some(first))?;
    }
    Ok(tree)
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
    /// oci: attach a provenance and/or SBOM attestation (`--provenance`, `--sbom`).
    pub attest: crate::export::attest::AttestRequest,
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

/// The attestations (`--provenance`, `--sbom`) of an image built from
/// `result`: in-toto statements whose subject is the image manifest. They
/// never change the image itself.
pub fn image_attestations(
    result: &BuildResult,
    manifest_digest: &str,
    manifest_bytes: &[u8],
    config_bytes: &[u8],
    root: &str,
    options: &BuildOptions,
    request: crate::export::attest::AttestRequest,
) -> Result<Vec<crate::export::attest::Referrer>> {
    use crate::export::attest::{build_facts, make_attestations, AttestSubject};
    if !request.any() {
        return Ok(Vec::new());
    }
    let facts = build_facts(result, &options.context_dir, options.sitefile.as_deref(), &options.build_args, options.platform.as_deref())?;
    let cfg = crate::export::oci::parse_config(config_bytes).map_err(|e| SitefileError::plain(E_STORE, format!("image config: {e}")))?;
    let title = cfg.labels.get(crate::export::oci::ann::TITLE).map(String::as_str);
    let subject = AttestSubject { digest: manifest_digest, size: manifest_bytes.len() as u64, files: &result.output, root, title };
    Ok(make_attestations(&subject, &facts, request, source_date_epoch()?))
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
    let span = begin(options.profiler.as_deref(), || "export".into(), "export", || vec![("format", json!(format.as_str()))]);
    let r = export_build(result, format, options, out_dir, target);
    span.end(Vec::new());
    r
}

fn export_build(result: BuildResult, format: ExportFormat, options: &BuildOptions, out_dir: &str, target: &ExportTarget) -> Result<ExportedBuild> {
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
        let referrers = image_attestations(&result, &img.manifest_digest, &img.manifest_bytes, &img.config_bytes, &img.root, options, target.attest)?;
        if out_dir.to_lowercase().ends_with(".tar") {
            let data = oci_archive(img.parts(), ref_name, source_date_epoch()?, &base_blobs, &referrers)?;
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
        let written = export_directory(&layout_tree(img.parts(), ref_name, &base_blobs, &referrers)?, out, &export_options)?;
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
    archive_limits: &'a ArchiveLimits,
    remote: RemoteEnv<'a>,
    memo: Option<&'a Mutex<BuildMemo>>,
}

/// What fetching a remote IMPORT needs: the network policy and the lockfile rules (and, for builds,
/// the vendor directory with the context reader for its blobs).
pub struct RemoteEnv<'a> {
    pub policy: &'a FetchPolicy,
    pub lock: &'a Mutex<Option<Lockfile>>,
    pub label: &'a str,
    pub warn: &'a Callback,
    pub frozen: bool,
    pub update: bool,
    /// Vendored copies (`sitefile vendor`), preferred over the network.
    pub vendor: Option<(&'a Vendor, &'a LocalContext)>,
}

/// The bytes of a remote IMPORT and its final URL, checked against its INTEGRITY and the lockfile (and
/// recorded there): read from the vendor directory when it has the URL, otherwise fetched.
pub fn fetch_remote_import(spec: &ImportSpec, url: &str, env: &RemoteEnv<'_>) -> Result<(Vec<u8>, String)> {
    let span = Some(&spec.span);
    let entry = env.lock.lock().ok().and_then(|l| l.as_ref().and_then(|l| l.get(url).cloned()));
    if env.frozen && entry.is_none() {
        return Err(SitefileError::at(E_LOCK_MISSING, format!("IMPORT {}: '{url}' is not in the lockfile (--frozen-lockfile)", spec.name), &spec.span));
    }
    let vendored = env.vendor.and_then(|(v, _)| v.resource(url));
    if let (Some((v, _)), None) = (env.vendor, vendored) {
        (env.warn)(&format!(
            "{}:{}: IMPORT {}: '{url}' is not vendored in {}; fetching it (run 'sitefile vendor' to add it)",
            env.label, spec.span.line, spec.name, v.dir
        ));
    }
    let (bytes, final_url) = match (env.vendor, vendored) {
        (Some((v, local)), Some(e)) => v.read_resource(local, url, e, span)?,
        _ => {
            let f = fetch_remote(url, env.policy, span)?;
            (f.bytes, f.final_url)
        }
    };
    let digest = sha256(&bytes);
    if let Some(i) = &spec.integrity {
        verify_integrity(&bytes, i, url, span)?;
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
        (env.warn)(&format!("{}:{}: IMPORT {} is not pinned; add INTEGRITY {digest} for a reproducible build", env.label, spec.span.line, spec.name));
    }
    if env.frozen && entry.as_ref().is_some_and(|e| e.integrity != digest) {
        return Err(SitefileError::at(E_LOCK_MISMATCH, format!("IMPORT {}: '{url}' does not match the lockfile (--frozen-lockfile)", spec.name), &spec.span));
    }
    if let Ok(mut l) = env.lock.lock() {
        if let Some(l) = l.as_mut() {
            l.record(url, LockEntry { integrity: digest, size: bytes.len() as u64, resolved: final_url.clone() });
        }
    }
    Ok((bytes, final_url))
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

/// Read an import. The key identifies the result for incremental rebuilds: the
/// import's spec (with its Sitefile position) and the digest of the bytes or tree
/// it read. With a memo, conversions (Markdown, data, archives) whose key is
/// unchanged are reused; a collection's documents are memoized one by one.
fn resolve_import(spec: &ImportSpec, env: &ImportEnv<'_>) -> Result<(Resource, String)> {
    let origin = format!("{}:{} IMPORT {}", env.remote.label, spec.span.line, spec.name);
    let span = Some(&spec.span);
    let id = if env.memo.is_some() { format!("{spec:?}") } else { String::new() };
    let key_of = |raw: &str| if env.memo.is_some() { memo_key(&["import", env.remote.label, &id, raw]) } else { String::new() };
    // `compute` gets the per-document memo (children of this import's entry).
    let convert = |key: &str, compute: &dyn Fn(Option<DocMemo>) -> Result<Resource>| -> Result<Resource> {
        let Some(memo) = env.memo else { return compute(None) };
        memoize(memo, key, None, |children| {
            let docs = |parts: &[&str], make: &mut dyn FnMut() -> Result<MarkdownDocument>| -> Result<MarkdownDocument> {
                let mut all = vec!["document"];
                all.extend_from_slice(parts);
                memoize(memo, &memo_key(&all), Some(children), |_| make())
            };
            compute(Some(&docs))
        })
    };
    match &spec.location {
        ImportLocation::Local { path: rel } => {
            if env.local.stat(rel, span)? == EntryKind::Directory {
                match spec.as_kind {
                    Some(ImportKind::Markdown) => {
                        if spec.integrity.is_some() {
                            return Err(integrity_on_dir(spec));
                        }
                        let Resource::Tree { tree, provenance } = env.local.read_tree(rel, &origin, span)? else { unreachable!() };
                        let key = key_of(&provenance.digest);
                        let res = convert(&key, &|docs| {
                            let list = markdown_collection(&tree, &trim_trailing_slashes(&spec.uri), span, docs)?;
                            Ok(Resource::Value { format: ValueFormat::Markdown, value: Value::Array(list.into_iter().map(Value::Object).collect()), provenance: provenance.clone() })
                        })?;
                        return Ok((res, key));
                    }
                    Some(k @ (ImportKind::Json | ImportKind::Yaml)) => {
                        if spec.integrity.is_some() {
                            return Err(integrity_on_dir(spec));
                        }
                        let Resource::Tree { tree, provenance } = env.local.read_tree(rel, &origin, span)? else { unreachable!() };
                        let (kind, format) = if k == ImportKind::Json { (DataKind::Json, ValueFormat::Json) } else { (DataKind::Yaml, ValueFormat::Yaml) };
                        let key = key_of(&provenance.digest);
                        let res = convert(&key, &|docs| {
                            let list = data_collection(&tree, kind, &trim_trailing_slashes(&spec.uri), span, docs)?;
                            Ok(Resource::Value { format, value: Value::Array(list.into_iter().map(Value::Object).collect()), provenance: provenance.clone() })
                        })?;
                        return Ok((res, key));
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
                let res = env.local.read_tree(rel, &origin, span)?;
                let key = key_of(&res.provenance().digest);
                return Ok((res, key));
            }
            let file = env.local.read_file(rel, span)?;
            let Resource::File { bytes, mode, basename, provenance } = &file else { unreachable!() };
            if let Some(i) = &spec.integrity {
                verify_integrity(bytes, i, &spec.uri, span)?;
            }
            let key = key_of(&format!("{}\0{mode}\0{basename}", provenance.digest));
            match spec.as_kind {
                None | Some(ImportKind::File) => Ok((file, key)),
                Some(kind) => {
                    let info = BytesInfo { uri: spec.uri.clone(), resolved_url: None, basename: basename.clone(), mode: Some(*mode) };
                    let res = convert(&key, &|_| bytes_to_resource(bytes.clone(), kind, &info, span, env.archive_limits))?;
                    Ok((res, key))
                }
            }
        }
        ImportLocation::Remote { url } => {
            let kind = spec.as_kind.unwrap_or(ImportKind::File);
            let (bytes, final_url) = fetch_remote_import(spec, url, &env.remote)?;
            let base = url_basename(&final_url);
            let basename = if base.is_empty() { spec.name.clone() } else { base };
            let key = key_of(&format!("{}\0{final_url}", sha256(&bytes)));
            let info = BytesInfo { uri: url.clone(), resolved_url: Some(final_url), basename, mode: None };
            let res = convert(&key, &|_| bytes_to_resource(bytes.clone(), kind, &info, span, env.archive_limits))?;
            Ok((res, key))
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
        /// Local files: the content digest, mode and name (memo keys); empty otherwise.
        digest: String,
    },
    Tree {
        tree: Tree,
        label: String,
        /// Local directories: the tree digest (memo keys); empty otherwise.
        digest: String,
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
    profiler: Option<&'a Profiler>,
    /// Incremental rebuilds (with `BuildOptions.memo`): the memo and the keys of what operations read.
    memo: Option<&'a Mutex<BuildMemo>>,
    /// Import name -> its key (resolve_import).
    resource_keys: HashMap<String, String>,
    /// The Sitefile label and its ARG/LET values.
    env_key: String,
    /// Stage name -> the key of its final tree.
    stage_keys: HashMap<String, String>,
    /// While an operation runs with a memo: the warnings it printed (a memoized step replays them).
    recorded: Option<Arc<Mutex<Vec<String>>>>,
    /// The warning callback that does not record (a tool's stderr).
    unrecorded: Warn,
    /// Resolve a `FROM image` stage base: (reference, AT, local, lead, span).
    from_image: &'a dyn Fn(&str, Option<&str>, bool, &str, &SourceSpan) -> Result<FromImage>,
}

/// What a stage starts from, for the trace.
fn stage_from(stage: &Stage) -> String {
    match &stage.base {
        StageBase::Scratch => "scratch".into(),
        StageBase::Stage { name } => name.clone(),
        StageBase::Local { path } if path.is_empty() => "./".into(),
        StageBase::Local { path } => format!("./{path}"),
        StageBase::Image { reference, at: Some(at), .. } => format!("image {reference} AT {at}"),
        StageBase::Image { reference, .. } => format!("image {reference}"),
    }
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

    /// The tree a stage starts from, and its key.
    fn start_stage(&self, stage: &Stage) -> Result<(Tree, String)> {
        match &stage.base {
            StageBase::Scratch => Ok((Tree::empty(), if self.memo.is_some() { memo_key(&["scratch", &self.env_key]) } else { String::new() })),
            StageBase::Stage { name } => {
                let tree = self.stages.get(name).cloned().ok_or_else(|| unknown_stage(name, &stage.span))?;
                Ok((tree, self.stage_keys.get(name).cloned().unwrap_or_default()))
            }
            StageBase::Local { path } => {
                let display = if path.is_empty() { "./".to_string() } else { format!("./{path}") };
                if self.local.stat(path, Some(&stage.span))? != EntryKind::Directory {
                    return Err(SitefileError::at(E_TYPE, format!("FROM '{display}' is a file; FROM needs a directory"), &stage.span));
                }
                match self.local.read_tree(path, &format!("{}:{} FROM", self.label, stage.span.line), Some(&stage.span))? {
                    Resource::Tree { tree, provenance } => {
                        let key = if self.memo.is_some() { memo_key(&["from", &self.env_key, &stage.span.line.to_string(), path, &provenance.digest]) } else { String::new() };
                        Ok((tree, key))
                    }
                    _ => unreachable!("read_tree returns a tree"),
                }
            }
            StageBase::Image { reference, local, at, lead, span } => {
                let img = (self.from_image)(reference, at.as_deref(), *local, lead, span)?;
                let key = if self.memo.is_some() {
                    memo_key(&["image", &self.env_key, &stage.span.line.to_string(), reference, at.as_deref().unwrap_or(""), &img.digest])
                } else {
                    String::new()
                };
                Ok((img.tree, key))
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
                digest: String::new(),
            }),
            SourceRef::Local { path, span } => {
                let label = format!("./{path}");
                if self.local.stat(path, Some(span))? == EntryKind::Directory {
                    let Resource::Tree { tree, provenance } = self.local.read_tree(path, origin, Some(span))? else { unreachable!() };
                    return Ok(Source::Tree { tree, label, digest: provenance.digest });
                }
                let Resource::File { bytes, mode, basename, provenance } = self.local.read_file(path, Some(span))? else { unreachable!() };
                let digest = format!("{}\0{mode}\0{basename}", provenance.digest);
                Ok(Source::File { bytes: Arc::from(bytes), mode, basename, label, inline: None, digest })
            }
            SourceRef::Stage { name, path, span } => {
                let tree = self.stages.get(name).ok_or_else(|| unknown_stage(name, span))?;
                let label = format!("--from={name} {path}");
                if let Some(node) = tree.get(path) {
                    return Ok(Source::File { bytes: node.bytes.clone(), mode: node.mode, basename: basename_virtual(path).to_string(), label, inline: None, digest: String::new() });
                }
                if tree.is_directory(path) {
                    return Ok(Source::Tree { tree: tree.subtree(path), label, digest: String::new() });
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
                        Ok(Source::File { bytes: Arc::from(bytes.as_slice()), mode: *mode, basename: basename.clone(), label, inline: None, digest: String::new() })
                    }
                    Resource::Tree { tree, .. } => {
                        if let Some(node) = tree.get(path) {
                            return Ok(Source::File { bytes: node.bytes.clone(), mode: node.mode, basename: basename_virtual(path).to_string(), label, inline: None, digest: String::new() });
                        }
                        if tree.is_directory(path) {
                            return Ok(Source::Tree { tree: tree.subtree(path), label, digest: String::new() });
                        }
                        Err(SitefileError::at(E_NOT_FOUND, format!("'{path}' does not exist in import '{name}'"), span))
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------ incremental rebuilds

    /// What a source reference reads, for memo keys (the operation's own text covers inline bodies and paths).
    fn source_key(&self, r: &SourceRef, src: &Source) -> String {
        let digest = match src {
            Source::File { digest, .. } | Source::Tree { digest, .. } => digest,
        };
        match r {
            SourceRef::Inline { .. } => "inline".into(),
            SourceRef::Local { .. } => format!("local\0{digest}"),
            SourceRef::Stage { name, .. } => format!("stage\0{}", self.stage_keys.get(name).map(String::as_str).unwrap_or("")),
            SourceRef::Import { name, .. } => format!("import\0{}", self.resource_keys.get(name).map(String::as_str).unwrap_or("")),
        }
    }

    /// The keys of the imports an operation may read, by name (ARG/LET values are in its op id).
    /// `except`: a loop variable, which hides an import of the same name.
    fn names_key(&self, names: BTreeSet<String>, except: Option<&str>) -> String {
        let mut parts: Vec<&str> = vec!["names"];
        for n in &names {
            if Some(n.as_str()) == except {
                continue;
            }
            parts.push(n);
            parts.push(self.resource_keys.get(n).map(String::as_str).unwrap_or(""));
        }
        memo_key(&parts)
    }

    /// A memoized step's files: replays the warnings it printed after those this run already
    /// printed before the lookup (the same code ran up to here, so they are a prefix).
    fn memo_lookup(&mut self, key: &Option<String>) -> Option<Produced> {
        let (Some(memo), Some(k)) = (self.memo, key) else { return None };
        let (files, warnings) = lock_memo(memo).get::<(Files, Vec<String>)>(k)?;
        let seen = self.recorded.as_ref().and_then(|r| r.lock().ok().map(|r| r.len())).unwrap_or(0);
        for w in warnings.iter().skip(seen) {
            (self.warn)(w);
        }
        Some(Produced { files, key: Some(k.clone()), hit: true })
    }

    /// Memoize a step's files under `key` (with the warnings it printed), unless `store` is false.
    fn memo_store(&mut self, key: Option<String>, files: Files, children: Vec<String>, store: bool) -> Produced {
        let (Some(memo), Some(k), true) = (self.memo, key, store) else { return Produced { files, key: None, hit: false } };
        let warnings = self.recorded.as_ref().and_then(|r| r.lock().ok().map(|r| r.clone())).unwrap_or_default();
        lock_memo(memo).set(k.clone(), (files.clone(), warnings), children);
        Produced { files, key: Some(k), hit: false }
    }

    /// `produce`, with the warnings it prints recorded (a memoized step replays them).
    fn produce_recorded(&mut self, op: &Operation, tree: &Tree, op_id: &str) -> Result<Produced> {
        let outer = self.warn.clone();
        let rec: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let r2 = rec.clone();
        let o2 = outer.clone();
        self.warn = Arc::new(move |m: &str| {
            if let Ok(mut r) = r2.lock() {
                r.push(m.to_string());
            }
            o2(m)
        });
        self.recorded = Some(rec);
        self.unrecorded = outer.clone();
        let r = self.produce(op, tree, op_id);
        self.warn = outer;
        self.recorded = None;
        r
    }

    /// Run one operation: produce its files, then add them to the tree. With a memo, a step
    /// whose key (the operation and everything it reads) is unchanged reuses its files, and
    /// one whose input tree is also unchanged reuses the resulting tree. The result's key
    /// chains the input tree's key with the step's, like image layers.
    fn execute_op(&mut self, op: &Operation, tree: Tree, prev: &str) -> Result<(Tree, String, Option<&'static str>)> {
        let span = op.span();
        let Some(memo) = self.memo else {
            if let Operation::Sitemap { .. } = op {
                return Ok((self.sitemap(op, tree)?, String::new(), None));
            }
            let p = self.produce(op, &tree, "")?;
            return Ok((tree.with_files(p.files, policy_of(op_overwrite(op)), Some(span))?, String::new(), None));
        };
        let op_id = memo_key(&["op", &self.env_key, &format!("{op:?}")]);
        if let Operation::Sitemap { .. } = op {
            // Reads the whole tree: only the chain key can say it is unchanged.
            let key = memo_key(&["chain", prev, &op_id]);
            if let Some(hit) = lock_memo(memo).get::<Tree>(&key) {
                return Ok((hit, key, Some("hit")));
            }
            let out = self.sitemap(op, tree)?;
            lock_memo(memo).set(key.clone(), out.clone(), Vec::new());
            return Ok((out, key, Some("miss")));
        }
        let p = self.produce_recorded(op, &tree, &op_id)?;
        // Not memoizable (a plugin that opted out of caching): chain on what it produced instead.
        let step_key = match &p.key {
            Some(k) => k.clone(),
            None => {
                let mut parts: Vec<String> = vec!["files".into()];
                for (f, n) in &p.files {
                    parts.extend([f.clone(), n.mode.to_string(), n.digest.clone(), n.origin.clone()]);
                }
                memo_key(&parts.iter().map(String::as_str).collect::<Vec<_>>())
            }
        };
        let key = memo_key(&["chain", prev, &step_key]);
        if let Some(hit) = lock_memo(memo).get::<Tree>(&key) {
            return Ok((hit, key, Some("hit")));
        }
        let out = tree.with_files(p.files, policy_of(op_overwrite(op)), Some(span))?;
        lock_memo(memo).set(key.clone(), out.clone(), Vec::new());
        Ok((out, key, Some(if p.hit { "files" } else { "miss" })))
    }

    fn sitemap(&self, op: &Operation, tree: Tree) -> Result<Tree> {
        let Operation::Sitemap { base, dest, overwrite, span } = op else { unreachable!() };
        let origin = format!("{}:{} SITEMAP", self.label, span.line);
        let xml = sitemap_xml(&tree, base);
        tree.with_files([(dest.clone(), make_file(xml.into_bytes(), origin, 0o644))], policy_of(*overwrite), Some(span))
    }

    /// The files an operation (other than SITEMAP) adds; `op_id` is its memo identity ("" without a memo).
    fn produce(&mut self, op: &Operation, tree: &Tree, op_id: &str) -> Result<Produced> {
        let origin = format!("{}:{} {}", self.label, op.span().line, op.keyword());
        let memoized = self.memo.is_some();
        match op {
            Operation::Transform { .. } => self.produce_transform(op, op_id),
            Operation::Feed { list, link, .. } => {
                // A feed reads its list and what its LINK names.
                let key = memoized.then(|| {
                    let mut names = operand_names(&[link]);
                    names.insert(list.clone());
                    memo_key(&["feed", op_id, &self.names_key(names, None)])
                });
                if let Some(hit) = self.memo_lookup(&key) {
                    return Ok(hit);
                }
                let files = self.feed_files(op)?;
                Ok(self.memo_store(key, files, Vec::new(), true))
            }
            Operation::Sitemap { .. } => unreachable!("SITEMAP reads the tree: execute_op runs it"),
            Operation::Copy { source, dest, dest_is_dir, span, .. } => {
                let src = self.read_source(source, &origin)?;
                let into_dir = matches!(src, Source::File { .. }) && (*dest_is_dir || tree.is_directory(dest));
                let key = memoized.then(|| {
                    let shape = if matches!(src, Source::File { .. }) { into_dir.to_string() } else { "tree".into() };
                    memo_key(&["copy", op_id, &self.source_key(source, &src), &shape])
                });
                if let Some(hit) = self.memo_lookup(&key) {
                    return Ok(hit);
                }
                let files: Files = match src {
                    Source::File { bytes, mode, basename, label, inline, .. } => {
                        if into_dir && inline.is_some() {
                            return Err(SitefileError::at(E_PATH, format!("'{dest}' is a directory; an inline {label} body needs a file destination"), span));
                        }
                        let target = if into_dir { join_virtual(dest, &basename) } else { dest.clone() };
                        vec![(target, make_file(bytes, origin, mode))]
                    }
                    Source::Tree { tree: src, label, .. } => {
                        if src.is_empty() {
                            (self.warn)(&format!("{origin}: '{label}' contains no files"));
                        }
                        src.mounted_at(dest)
                            .into_iter()
                            .map(|(p, n)| (p, Arc::new(FileNode { bytes: n.bytes.clone(), mode: n.mode, digest: n.digest.clone(), origin: origin.clone() })))
                            .collect()
                    }
                };
                Ok(self.memo_store(key, files, Vec::new(), true))
            }
            Operation::Render { template, dest, each, escape, span, .. } => {
                let src = self.read_source(template, &origin)?;
                let template_key = if memoized { self.source_key(template, &src) } else { String::new() };
                let Source::File { bytes, inline, label, .. } = src else {
                    return Err(SitefileError::at(E_TYPE, format!("RENDER template '{}' is a directory, not a file", src.label()), template.span()));
                };
                let text = decode_utf8(&bytes, &format!("template '{label}'"), Some(template.span()))?;
                let parts = match &inline {
                    Some((file, first)) => parse_render_template(&text, file, *first)?,
                    None => parse_render_template(&text, &label, 1)?,
                };
                // What the template names, the FOR list and the destination's variables (ARG/LET values are in op_id).
                let key = memoized.then(|| {
                    let mut names = template_names(&parts);
                    if let Some(e) = each {
                        names.insert(e.list.clone());
                        names.extend(operand_names(&[&e.dest]));
                    }
                    memo_key(&["render", op_id, &template_key, &self.names_key(names, None)])
                });
                if let Some(hit) = self.memo_lookup(&key) {
                    return Ok(hit);
                }
                let scope = self.scope();
                let Some(each) = each else {
                    let out = render_template(
                        &parts,
                        &|n: &str, p: &[String], s: &SourceSpan| scope.lookup(n, p, s),
                        *escape,
                        Some(&|n: &str, p: &[String], s: &SourceSpan, l: bool| scope.value_of(n, p, s, l)),
                    )?;
                    let files = vec![(dest.clone(), make_file(out.into_bytes(), origin, 0o644))];
                    return Ok(self.memo_store(key, files, Vec::new(), true));
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
                // An element's page depends on the template, the element and the imports the template
                // names (other than the loop variable), so editing one document re-renders one page.
                let refs_key = if memoized { self.names_key(template_names(&parts), Some(as_name)) } else { String::new() };
                let children: Mutex<Vec<String>> = Mutex::new(Vec::new());
                let mut files: Files = Vec::new();
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
                    let page = || -> Result<Node> {
                        let out = render_template(
                            &parts,
                            &|n: &str, p: &[String], s: &SourceSpan| item_scope.lookup(n, p, s),
                            *escape,
                            Some(&|n: &str, p: &[String], s: &SourceSpan, l: bool| item_scope.value_of(n, p, s, l)),
                        )?;
                        Ok(make_file(out.into_bytes(), origin.clone(), 0o644))
                    };
                    let node = match self.memo {
                        Some(memo) => {
                            let item_key = memo_key(&["item", op_id, &template_key, &refs_key, &value_key(item)]);
                            memoize(memo, &item_key, Some(&children), |_| page())?
                        }
                        None => page()?,
                    };
                    files.push((target.path, node));
                }
                Ok(self.memo_store(key, files, children.into_inner().unwrap_or_default(), true))
            }
        }
    }

    fn feed_files(&mut self, op: &Operation) -> Result<Files> {
        let Operation::Feed { list, base, link, dest, title, span, .. } = op else { unreachable!() };
        let origin = format!("{}:{} FEED", self.label, span.line);
        let scope = self.scope();
        let items = scope.value_of(list, &[], span, false)?;
        let Some(Value::Array(items)) = items.as_deref() else {
            return Err(SitefileError::at(E_TYPE, format!("FEED {list}: '{list}' is not a list (import a directory AS markdown)"), span));
        };
        if items.is_empty() {
            (self.warn)(&format!("{origin}: '{list}' is empty, so the feed has no entries"));
        }
        let mut entries = Vec::new();
        for (i, item) in items.iter().enumerate() {
            let Value::Object(doc) = item else {
                return Err(SitefileError::at(
                    E_TYPE,
                    format!("FEED {list}: element {} is not a document (an object with title and date)", i + 1),
                    span,
                ));
            };
            let name = match doc.get("path") {
                Some(Value::String(p)) => p.clone(),
                _ => format!("#{}", i + 1),
            };
            let text = |key: &str| -> Result<Option<String>> {
                match doc.get(key) {
                    None | Some(Value::Null) => Ok(None),
                    Some(v @ (Value::String(_) | Value::Number(_) | Value::Bool(_))) => Ok(Some(js_string_of(v))),
                    Some(_) => Err(SitefileError::at(E_TYPE, format!("FEED {list}: '{key}' of document '{name}' is not text"), span)),
                }
            };
            let required = |key: &str| -> Result<String> {
                text(key)?.ok_or_else(|| {
                    SitefileError::at(E_VALIDATION, format!("FEED {list}: document '{name}' has no '{key}' (add it to its frontmatter)"), span)
                })
            };
            let title = required("title")?;
            let raw_date = required("date")?;
            let Some(date) = parse_feed_date(&raw_date) else {
                return Err(SitefileError::at(
                    E_VALIDATION,
                    format!("FEED {list}: document '{name}' has date '{raw_date}'; expected YYYY-MM-DD or an RFC 3339 date-time"),
                    span,
                ));
            };
            let raw = scope.with_item(FEED_ITEM, item).interpolate(link)?;
            let l = normalize_virtual(&raw, Some(&link.span), "FEED LINK")?;
            let path = if l.path == "/" || !l.trailing_slash { l.path } else { format!("{}/", l.path) };
            let author = text("author")?.filter(|a| !a.is_empty());
            let summary = text("excerpt")?.filter(|s| !s.is_empty());
            let content = text("content")?;
            entries.push(FeedEntry { title, url: page_url(base, &path), date, author, summary, content });
        }
        let xml = atom_xml(title, &format!("{base}/"), &page_url(base, dest), &entries);
        Ok(vec![(dest.clone(), make_file(xml.into_bytes(), origin, 0o644))])
    }

    fn produce_transform(&mut self, op: &Operation, op_id: &str) -> Result<Produced> {
        let Operation::Transform { source, plugin, dest, options, each, args, span, .. } = op else { unreachable!() };
        let origin = format!("{}:{} TRANSFORM", self.label, span.line);
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
        let mut src_key = String::new();
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
                if self.memo.is_some() {
                    src_key = self.source_key(src_ref, &src);
                }
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
        if (self.cache.is_some() || self.memo.is_some()) && tool.is_none() {
            if !self.plugin_digests.contains_key(plugin) {
                // A wasm plugin is just its module; a JS plugin can read its whole folder.
                let digest = match self.plugins.wasm_bytes(plugin) {
                    Some(w) => Some(sha256(w)),
                    None => match self.plugins.folder_of(plugin) {
                        Some(dir) => digest_folder(&dir).ok().flatten(),
                        None => None,
                    },
                };
                if digest.is_none() && self.cache.is_some() {
                    (self.warn)(&format!("{origin}: PLUGIN {plugin}'s folder is too large to hash; its results are not cached"));
                }
                self.plugin_digests.insert(plugin.clone(), digest);
            }
            plugin_digest = self.plugin_digests.get(plugin).cloned().flatten();
        }

        // Incremental rebuilds: the same code (plugin digest, or tool binary), source, data, list and interpolated names.
        let code = match (&tool, &plugin_digest) {
            (Some(t), _) => Some(format!("tool\0{}\0{}\0{}", t.path.display(), t.size, t.mtime_ms)),
            (None, Some(d)) => Some(format!("plugin\0{d}")),
            (None, None) => None,
        };
        let memo = self.memo.filter(|_| code.is_some());
        let code = code.unwrap_or_default();
        let mut operand_refs = {
            let mut ops: Vec<&Operand> = vec![dest];
            ops.extend(options.iter().map(|o| &o.value));
            ops.extend(args.as_deref().unwrap_or(&[]).iter());
            operand_names(&ops)
        };
        if let (Some(_), Some(SourceRef::Import { name, .. })) = (&data, source) {
            operand_refs.insert(name.clone());
        }
        let key = memo.map(|_| {
            let mut names = operand_refs.clone();
            if let Some(e) = each {
                names.insert(e.list.clone());
            }
            memo_key(&["transform", op_id, &src_key, &code, &self.names_key(names, None)])
        });
        // One FOR element's files: the same request gives the same result (the list itself is not read).
        let item_base = match (memo, each) {
            (Some(_), Some(e)) => Some(memo_key(&["transform-item", op_id, &src_key, &code, &self.names_key(operand_refs, Some(&e.as_name))])),
            _ => None,
        };
        if let Some(hit) = self.memo_lookup(&key) {
            return Ok(hit);
        }

        let mut children: Vec<String> = Vec::new();
        let mut files: Files = Vec::new();
        for item in items {
            let item_key = match (&item_base, item) {
                (Some(b), Some(v)) => Some(memo_key(&[b, &value_key(v)])),
                _ => None,
            };
            if let (Some(m), Some(k)) = (memo, &item_key) {
                children.push(k.clone());
                if let Some(reused) = lock_memo(m).get::<Files>(k) {
                    files.extend(reused);
                    continue;
                }
            }
            let mut item_files: Files = Vec::new();
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
            let cached = if cacheable {
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
            let call = begin(
                self.profiler,
                || format!("TRANSFORM {plugin} {}", d.path),
                "transform",
                || vec![("plugin", json!(plugin)), ("tool", json!(tool.is_some())), ("dest", json!(d.path))],
            );
            let mut result: Option<PluginResult> = match (&cached, self.cache.as_mut()) {
                (Some(k), Some(c)) => c.get(k),
                _ => None,
            };
            let cache_state = if cached.is_none() { "off" } else if result.is_some() { "hit" } else { "miss" };
            if result.is_none() {
                let r = if let Some(t) = &tool {
                    self.stats.tool_runs += 1;
                    let call = ToolCall {
                        args: argv.clone().unwrap_or_default(),
                        dest: d.path.clone(),
                        options: opts.clone(),
                        input: input.as_ref().map(|i| ToolInput { name: i.name.clone(), bytes: i.bytes.clone() }),
                    };
                    // The tool's stderr, like a TRANSFORM cache hit, is not replayed by a memo hit.
                    let warn = self.unrecorded.clone();
                    run_tool(t, &call, &self.tool_policy, &move |l: &str| warn(l), Some(span))?
                } else {
                    self.plugins.call(plugin, &request, Some(span))?
                };
                if let Some(k) = &cached {
                    if tool.is_some() || self.plugins.cacheable(plugin) {
                        if let Some(c) = self.cache.as_mut() {
                            c.put(k, &r);
                        }
                    }
                }
                result = Some(r);
            }
            call.end(vec![("cache", json!(cache_state))]);
            match result.expect("set above") {
                PluginResult::File(bytes) => {
                    if d.trailing_slash {
                        return Err(SitefileError::at(
                            E_PLUGIN,
                            format!("PLUGIN {plugin} returned one file, but '{raw_dest}' is a directory destination"),
                            &dest.span,
                        ));
                    }
                    item_files.push((d.path.clone(), make_file(bytes, origin.clone(), 0o644)));
                }
                PluginResult::Files(list) => {
                    // { files }: placed under the destination directory.
                    for (rel, bytes) in list {
                        let base = if d.path == "/" { "" } else { d.path.as_str() };
                        let p = normalize_virtual(&format!("{base}/{rel}"), Some(&dest.span), &format!("file '{rel}' returned by PLUGIN {plugin}"))?.path;
                        if rel.contains("..") || rel.starts_with('/') {
                            return Err(SitefileError::at(E_PLUGIN, format!("PLUGIN {plugin} returned an unsafe path '{rel}'"), span));
                        }
                        item_files.push((p, make_file(bytes, origin.clone(), 0o644)));
                    }
                }
            }
            // A plugin that answered `cache: false` is not reused either.
            if let (Some(m), Some(k)) = (memo, item_key) {
                if tool.is_some() || self.plugins.cacheable(plugin) {
                    lock_memo(m).set(k, item_files.clone(), Vec::new());
                }
            }
            files.extend(item_files);
        }
        let store = tool.is_some() || self.plugins.cacheable(plugin);
        Ok(self.memo_store(key, files, children, store))
    }
}

type Files = Vec<(String, Node)>;

/// An operation's files, before they are added to the tree; `key`: memoized under it.
struct Produced {
    files: Files,
    key: Option<String>,
    hit: bool,
}

fn op_overwrite(op: &Operation) -> bool {
    match op {
        Operation::Copy { overwrite, .. }
        | Operation::Render { overwrite, .. }
        | Operation::Transform { overwrite, .. }
        | Operation::Feed { overwrite, .. }
        | Operation::Sitemap { overwrite, .. } => *overwrite,
    }
}

/// Names an operand interpolates.
fn operand_names(ops: &[&Operand]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for op in ops {
        for part in &op.parts {
            if let TemplatePart::Var { name, .. } = part {
                out.insert(name.clone());
            }
        }
    }
    out
}

/// Top-level names a template reads (variables, #each lists, #if conditions).
fn template_names(nodes: &[TemplateNode]) -> BTreeSet<String> {
    fn walk_nodes(nodes: &[TemplateNode], out: &mut BTreeSet<String>) {
        for n in nodes {
            match n {
                TemplateNode::Text(_) => {}
                TemplateNode::Var { name, .. } => {
                    out.insert(name.clone());
                }
                TemplateNode::Each { list, body, empty, .. } => {
                    out.insert(list.name.clone());
                    walk_nodes(body, out);
                    walk_nodes(empty, out);
                }
                TemplateNode::If { cond, then, otherwise, .. } => {
                    out.insert(cond.name.clone());
                    walk_nodes(then, out);
                    walk_nodes(otherwise, out);
                }
            }
        }
    }
    let mut out = BTreeSet::new();
    walk_nodes(nodes, &mut out);
    out
}

/// A FOR element's identity in memo keys (serde_json keeps key order; values cannot hold NaN).
fn value_key(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
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

    fn strict_ctx(sitefile: &str, lock: Option<&str>) -> (PathBuf, BuildOptions) {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!("sitefile-pins-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Sitefile"), sitefile).unwrap();
        if let Some(l) = lock {
            std::fs::write(dir.join("Sitefile.lock"), l).unwrap();
        }
        let mut o = BuildOptions { context_dir: dir.display().to_string(), strict_pins: true, ..Default::default() };
        // Nothing listens on port 9: a fetch fails with E_FETCH, so E_UNPINNED proves the check ran first.
        o.fetch_policy.allow_private_network = true;
        (dir, o)
    }

    const URL: &str = "https://127.0.0.1:9/lib.js";
    const PIN: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000000";

    #[test]
    fn strict_pins_refuses_unpinned_imports_before_fetching() {
        let (dir, o) = strict_ctx(&format!("FROM scratch\nIMPORT p FROM ./\nIMPORT lib FROM {URL}\nEXPORT /\n"), None);
        let e = build(&o).err().unwrap();
        assert_eq!(e.code, E_UNPINNED);
        assert_eq!(e.span.as_ref().map(|s| s.line), Some(3));
        assert!(e.detail.starts_with(&format!("IMPORT lib: '{URL}' is not pinned (--strict-pins); add INTEGRITY sha256:<hex>")), "{}", e.detail);
        // INTEGRITY is a pin: the build gets as far as fetching
        let (_, o) = strict_ctx(&format!("FROM scratch\nIMPORT lib FROM {URL} INTEGRITY {PIN}\nEXPORT /\n"), None);
        assert_eq!(build(&o).err().unwrap().code, E_FETCH);
        // so is a lockfile entry, unless --no-lockfile
        let lock = format!(r#"{{"version":1,"resources":{{"{URL}":{{"integrity":"{PIN}","size":1,"resolved":"{URL}"}}}}}}"#);
        let (_, o) = strict_ctx(&format!("FROM scratch\nIMPORT lib FROM {URL}\nEXPORT /\n"), Some(&lock));
        assert_eq!(build(&o).err().unwrap().code, E_FETCH);
        assert!(check_plan_pins(&o, &plan_build(&o).unwrap()).is_ok());
        let o = BuildOptions { lockfile: LockfileSetting::Disabled, ..o };
        assert_eq!(build(&o).err().unwrap().code, E_UNPINNED);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn strict_pins_checks_the_base_image() {
        let tagged = "FROM scratch\nEXPORT / AS oci BASE 127.0.0.1:9/lib/base:1\n";
        let (_, o) = strict_ctx(tagged, None);
        let e = check_plan_pins(&o, &plan_build(&o).unwrap()).unwrap_err();
        assert_eq!(e.code, E_UNPINNED);
        assert_eq!(
            e.detail,
            "BASE 127.0.0.1:9/lib/base:1: '127.0.0.1:9/lib/base:1' is not pinned (--strict-pins); pin it with @sha256:..., or record it in Sitefile.lock with a build without --strict-pins"
        );
        // skipped bases (non-oci exports, inspect) are not checked
        let skip = BuildOptions { skip_base: true, ..o.clone() };
        assert!(check_plan_pins(&skip, &plan_build(&skip).unwrap()).is_ok());
        // a lockfile entry or a digest pins it; --base is checked like the Sitefile's BASE
        let lock = format!(r#"{{"version":1,"resources":{{}},"images":{{"127.0.0.1:9/lib/base:1":{{"digest":"{PIN}"}}}}}}"#);
        let (_, o) = strict_ctx(tagged, Some(&lock));
        assert!(check_plan_pins(&o, &plan_build(&o).unwrap()).is_ok());
        let other = BuildOptions { base_image: Some("127.0.0.1:9/lib/base:2".into()), ..o };
        assert_eq!(check_plan_pins(&other, &plan_build(&other).unwrap()).unwrap_err().code, E_UNPINNED);
        let (_, o) = strict_ctx(&format!("FROM scratch\nEXPORT / AS oci BASE 127.0.0.1:9/lib/base:1@{PIN}\n"), None);
        assert!(check_plan_pins(&o, &plan_build(&o).unwrap()).is_ok());
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

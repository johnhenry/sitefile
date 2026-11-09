//! Run a build (port of `src/executor.ts`): read the Sitefile, resolve the
//! imports, execute every stage's operations and export the final tree.
//!
//! Messages and warnings match the TypeScript implementation word for word;
//! `../conformance` checks it.

use crate::base_image::{
    base_ref, base_ref_as, read_layer_blobs, resolve_base, resolve_from_image, FromImage,
    ResolveBaseOptions,
};
use crate::build_args::BuildArgs;
use crate::canonical::{canonical, CanonicalOp};
use crate::checklinks::{broken_links, report_broken_links, CheckLinksOp};
use crate::digest::sha256;
use crate::documents::{
    data_collection, markdown_collection, sort_documents, DataKind, MarkdownDocument,
};
use crate::error::{ErrorCode::*, Needs, Result, SitefileError, SourceSpan};
use crate::export::archive_writers::{source_date_epoch, WriterOptions};
use crate::export::exporter::{
    export_directory, export_file, is_exporter_temp_name, resolve_path, ExportOptions,
};
use crate::export::formats::{encode_tree, is_archive_format, EncodeOptions};
use crate::export::oci::{
    build_oci_image, image_meta, layout_tree, oci_archive, BaseImage, OciOptions,
};
use crate::fingerprint::{compose_renames, fingerprint, FingerprintOp};
use crate::language::ast::{
    ExportFormat, ImportKind, Instruction, OciCompression, Operand, Program, TemplatePart,
    FEED_ITEM,
};
use crate::language::validate::parse_program;
use crate::memo::DocMemo;
use crate::memo::{lock as lock_memo, memo_key, memoize, BuildMemo, MemoStats, SharedMemo};
use crate::paths::{basename_virtual, join_virtual, normalize_virtual};
use crate::planner::{
    imported_stage_refs, plan, plan_with, target_plan, FragmentSpec, ImportLocation, ImportSpec,
    ImportedStageRef, Operation, Plan, PluginSpec, SourceRef, Stage, StageBase,
};
use crate::plugins::cache::{cache_key, digest_folder, CacheStats, TransformCache};
use crate::plugins::exec::{
    resolve_tool, run_tool, ResolvedTool, ToolCall, ToolInput, ToolPolicy, DEFAULT_TOOL_POLICY,
};
use crate::plugins::host::{
    run_bounded, transform_jobs, PluginHost, PluginInput, PluginPolicy, PluginRequest,
    PluginResult, DEFAULT_PLUGIN_POLICY,
};
use crate::plugins::wasm::check_wasm_plugin_in;
use crate::precache::{precache, PrecacheOp};
use crate::precompress::{precompress, PrecompressOp};
use crate::profile::{begin, Profiler};
use crate::registry::RegistryOptions;
use crate::render::{
    decode_utf8, fragment_not_value, parse_render_template, render_template_with,
    resolve_json_path, walk, TemplateContext, TemplateNode,
};
use crate::resources::archive::{os_metadata_warning, without_os_metadata};
use crate::resources::convert::{bytes_to_resource, verify_integrity, BytesInfo};
use crate::resources::git::{
    fetch_git_commit, pinned_git_url, select_git_path, GitLocation, GitSelection, GitTransport,
};
use crate::resources::https::{fetch_remote_with, FetchRequest};
use crate::resources::ignore::IgnoreRules;
use crate::resources::local::{EntryKind, LocalContext, LocalContextOptions};
use crate::resources::lockfile::{GitLockEntry, LockEntry, Lockfile};
use crate::resources::types::{
    ArchiveLimits, FetchPolicy, Resource, ValueFormat, DEFAULT_ARCHIVE_LIMITS, DEFAULT_FETCH_POLICY,
};
use crate::resources::vendor::{find_vendor_dir, Vendor};
use crate::secrets::{redact_error, redact_secrets, secret_headers, Secrets};
use crate::site::{
    atom_xml, cache_header_rules, headers_file, page_url, parse_feed_date, redirects_file,
    sitemap_files, FeedEntry, SITEMAP_LIMITS,
};
use crate::store::Store;
use crate::tree::{make_file, ConflictPolicy, FileNode, Node, Tree};
use crate::values::{js_string_of, Env, Scalar};
use serde_json::{json, Map, Value};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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
    /// How messages name the Sitefile (default: its path relative to the working directory).
    pub label: Option<String>,
    /// Build secrets by id (`--secret id=…,env=…|src=…`, see src/secrets.rs): sent only as the request
    /// headers that `IMPORT … SECRET` clauses name, and redacted from every warning and error.
    pub secrets: Secrets,
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
    /// `--node` / `SITEFILE_NODE`: the absolute path of the Node binary JavaScript plugins run on
    /// (default: `node` on PATH). Checked to be at least `MIN_NODE` before the first plugin starts.
    pub node: Option<PathBuf>,
    /// Permit hard-linked files in the context.
    pub allow_hardlinks: bool,
    /// Keep OS metadata (`__MACOSX/`, `._*`, `.DS_Store`, `Thumbs.db`, `desktop.ini`, `Icon\r`) in
    /// archives imported AS tree (`--keep-os-metadata`). Default false: dropped with a warning.
    pub keep_os_metadata: bool,
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
    /// The image store home (`$SITEFILE_HOME`) that `FROM <local image>`, `BASE`
    /// and the wasm compiled-module cache use. `None` (the CLI): `resolve_store_home()`,
    /// i.e. `$SITEFILE_HOME` or `~/.sitefile`. The TRANSFORM cache is `cache_dir`.
    pub store_home: Option<PathBuf>,
    /// Stops the build: checked before each stage, operation, plugin or tool call,
    /// import and download; a cancelled build fails with `E_CANCELLED`.
    pub cancel: Option<CancelToken>,
}

/// A handle that cancels a running build ([`BuildOptions::cancel`]). Clones
/// share one flag, so keep a clone and call [`cancel`](Self::cancel) from any thread.
/// Cancellation is cooperative: a plugin call, tool run or download already
/// running finishes (or times out) first, and nothing after it starts.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<std::sync::atomic::AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
    /// `Err(E_CANCELLED)` once cancelled.
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(SitefileError::cancelled())
        } else {
            Ok(())
        }
    }
}

impl BuildOptions {
    /// The store home this build uses (`store_home`, else `$SITEFILE_HOME` / `~/.sitefile`).
    pub fn store_home(&self) -> PathBuf {
        match &self.store_home {
            Some(h) => h.clone(),
            None => crate::store::resolve_store_home(),
        }
    }
    /// The image store this build reads `FROM` images and BASE layers from (created if needed).
    pub fn open_store(&self) -> Result<Store> {
        Store::open(self.store_home())
    }
    /// `Err(E_CANCELLED)` if the build's [`CancelToken`] was cancelled.
    pub fn check_cancelled(&self) -> Result<()> {
        match &self.cancel {
            Some(c) => c.check(),
            None => Ok(()),
        }
    }
}

impl Default for BuildOptions {
    fn default() -> Self {
        BuildOptions {
            context_dir: ".".into(),
            sitefile: None,
            label: None,
            secrets: Secrets::default(),
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
            node: None,
            allow_hardlinks: false,
            keep_os_metadata: false,
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
            store_home: None,
            cancel: None,
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
    /// The Sitefiles imported AS sitefile, by import name (each with its own imports, plan and, with `record_steps`, steps).
    pub imported: Vec<(String, ImportedSitefile)>,
    /// What each FINGERPRINT renamed (old path, new path), by its line.
    pub renames: Vec<(u32, Vec<(String, String)>)>,
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
    /// Set for a stage of an imported Sitefile: the import's qualified name (`theme`).
    pub import: Option<String>,
}

pub struct Loaded {
    pub program: Program,
    pub sitefile_path: PathBuf,
    pub label: String,
    pub source: String,
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
    let joined = if a.is_empty() {
        b.to_string()
    } else if b.is_empty() {
        a.to_string()
    } else {
        format!("{a}/{b}")
    };
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
    let mut s = if abs {
        format!("/{body}")
    } else if body.is_empty() {
        ".".into()
    } else {
        body
    };
    if p.ends_with('/') && !s.ends_with('/') {
        s.push('/');
    }
    s
}

/// Read, parse and validate a Sitefile.
pub fn load_program(
    context_dir: &str,
    sitefile: Option<&str>,
    label: Option<&str>,
) -> Result<Loaded> {
    let given = match sitefile {
        Some(s) => s.to_string(),
        None => join_path(context_dir, "Sitefile"),
    };
    let sitefile_path = resolve_path(Path::new(&given));
    let source = std::fs::read(&sitefile_path)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        // A Git context names its Sitefile by the label (the checkout is a temporary directory).
        .map_err(|_| {
            SitefileError::plain(
                E_NOT_FOUND,
                format!(
                    "cannot read Sitefile '{}'",
                    label
                        .map(str::to_string)
                        .unwrap_or_else(|| sitefile_path.display().to_string())
                ),
            )
        })?;
    let rel = relative_path(&cwd(), &sitefile_path);
    let label = if let Some(l) = label {
        l.to_string()
    } else if rel.is_empty() {
        sitefile_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        rel
    };
    Ok(Loaded {
        program: parse_program(&source, &label)?,
        sitefile_path,
        label,
        source,
    })
}

/// Parse, validate and evaluate variables, without reading any sources.
pub fn plan_build(options: &BuildOptions) -> Result<Plan> {
    let loaded = load_program(
        &options.context_dir,
        options.sitefile.as_deref(),
        options.label.as_deref(),
    )?;
    plan_for(&loaded.program, options)
}

/// `validate`'s lint warnings (`crate::lint`) for the whole Sitefile (a `--target` does not narrow them).
pub fn lint_build(options: &BuildOptions, disabled: &[String]) -> Result<Vec<String>> {
    let loaded = load_program(
        &options.context_dir,
        options.sitefile.as_deref(),
        options.label.as_deref(),
    )?;
    let p = plan(&loaded.program, &options.build_args)?;
    Ok(crate::lint::lint(
        &loaded.program,
        &p,
        &loaded.source,
        disabled,
    ))
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
        LockfileSetting::Default => Some(
            sitefile_path
                .parent()
                .unwrap_or(Path::new("/"))
                .join("Sitefile.lock"),
        ),
    }
}

/// The BASE image a build resolves: `--base`, else `EXPORT ... AS oci BASE`, unless skipped.
fn base_ref_of(options: &BuildOptions, p: &Plan) -> Option<String> {
    if options.skip_base {
        return None;
    }
    options.base_image.clone().or_else(|| {
        if p.export_format == ExportFormat::Oci {
            p.export_base.clone()
        } else {
            None
        }
    })
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
        self.imports
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, b)| *b)
    }
}

/// `--strict-pins`: every remote input must be pinned, so the build is reproducible. A remote IMPORT
/// needs INTEGRITY or a `Sitefile.lock` entry; a BASE image needs `@sha256:` or a lockfile `images`
/// entry. `lock` is `None` with --no-lockfile (only the Sitefile's own pins count then). Checked in
/// source order, before anything is fetched; the first unpinned input fails with E_UNPINNED.
pub fn check_pins(p: &Plan, lock: Option<&Lockfile>, base: Option<&str>) -> Result<Pins> {
    let mut pins = Pins::default();
    for spec in &p.imports {
        if let ImportLocation::Git(g) = &spec.location {
            if g.commit.is_some() {
                pins.imports.push((spec.name.clone(), "commit"));
                continue;
            }
            if lock.is_some_and(|l| l.get_git(&g.url).is_some()) {
                pins.imports.push((spec.name.clone(), "lockfile"));
                continue;
            }
            return Err(SitefileError::at(
                E_UNPINNED,
                format!(
                    "IMPORT {}: '{}' is not pinned (--strict-pins); name a commit (#<40 hex>[:path]), or record it in Sitefile.lock with a build without --strict-pins",
                    spec.name, g.url
                ),
                &spec.span,
            ));
        }
        let ImportLocation::Remote { url } = &spec.location else {
            continue;
        };
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
        let StageBase::Image {
            reference,
            local: false,
            lead,
            span,
            ..
        } = &st.base
        else {
            continue;
        };
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
    let sitefile_path = resolve_path(Path::new(
        &options
            .sitefile
            .clone()
            .unwrap_or_else(|| join_path(&options.context_dir, "Sitefile")),
    ));
    let lock = match lock_path_for(&options.lockfile, &sitefile_path) {
        Some(path) => Some(Lockfile::load(path)?),
        None => None,
    };
    check_pins(p, lock.as_ref(), base_ref_of(options, p).as_deref()).map(|_| ())
}

/// Run a build in memory.
pub fn build(options: &BuildOptions) -> Result<BuildResult> {
    // Build secrets never appear in a warning or an error, even when a server echoes one back.
    if options.secrets.is_empty() {
        return build_unredacted(options);
    }
    let secrets = options.secrets.clone();
    let mut redacted = options.clone();
    if let Some(on) = options.on_warning.clone() {
        let s = secrets.clone();
        redacted.on_warning = Some(Arc::new(move |m: &str| on(&redact_secrets(m, &s))));
    }
    match build_unredacted(&redacted) {
        Ok(r) => {
            let warnings = r
                .warnings
                .iter()
                .map(|w| redact_secrets(w, &secrets))
                .collect();
            Ok(BuildResult { warnings, ..r })
        }
        Err(e) => Err(redact_error(e, &secrets)),
    }
}

/// `--secret` ids that no IMPORT … SECRET names (likely a typo).
pub fn warn_unused_secrets(p: &Plan, secrets: &Secrets, warn: &Callback) {
    let used: std::collections::HashSet<&str> = p
        .imports
        .iter()
        .flat_map(|s| s.secrets.iter().map(|x| x.id.as_str()))
        .collect();
    let mut ids: Vec<&str> = secrets.ids().collect();
    ids.sort();
    for id in ids {
        if !used.contains(id) {
            warn(&format!("--secret {id} is not used by any IMPORT … SECRET"));
        }
    }
}

fn build_unredacted(options: &BuildOptions) -> Result<BuildResult> {
    let Some(memo) = &options.memo else {
        return build_once(options);
    };
    lock_memo(memo).begin();
    let r = build_once(options);
    let mut m = lock_memo(memo);
    m.finish(r.is_ok());
    let stats = m.stats;
    r.map(|r| BuildResult {
        memo_stats: Some(stats),
        ..r
    })
}

fn build_once(options: &BuildOptions) -> Result<BuildResult> {
    // Git imports AS sitefile are checked out into temporary directories, removed when the build ends.
    let temp_dirs: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    let r = build_with(options, &temp_dirs);
    for d in temp_dirs.into_inner().unwrap_or_default() {
        let _ = std::fs::remove_dir_all(d);
    }
    r
}

fn build_with(options: &BuildOptions, temp_dirs: &Mutex<Vec<PathBuf>>) -> Result<BuildResult> {
    let prof = options.profiler.as_deref();
    let parse_span = begin(prof, || "parse".into(), "build", Vec::new);
    let Loaded {
        program,
        sitefile_path,
        label,
        ..
    } = load_program(
        &options.context_dir,
        options.sitefile.as_deref(),
        options.label.as_deref(),
    )?;
    let full = plan(&program, &options.build_args)?;
    let all_stages = full.stages.clone();
    let p = match &options.target {
        Some(t) => target_plan(full, t)?,
        None => full,
    };
    let mut parse_args = vec![
        ("imports", json!(p.imports.len())),
        ("stages", json!(p.stages.len())),
    ];
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
    warn_unused_secrets(&p, &options.secrets, &warn);
    let tools = resolve_tools(&p, options, &warn)?;
    let plugin_permission = check_plugins(&p, options, &warn)?;

    let context_real = std::fs::canonicalize(&options.context_dir).map_err(|_| {
        SitefileError::plain(
            E_NOT_FOUND,
            format!("build context '{}' does not exist", options.context_dir),
        )
    })?;
    let mut ignore = vec![".git".to_string()];
    // The same paths, absolute: an imported Sitefile's context inside this one skips them too.
    let mut skip: Vec<PathBuf> = Vec::new();
    if let Some(out) = &options.out_dir {
        let out_real = realpath_loose(&resolve_path(Path::new(out)));
        let rel = relative_path(&context_real, &out_real);
        if !rel.is_empty() && !rel.starts_with("..") && !rel.starts_with('/') {
            ignore.push(rel);
            skip.push(out_real.clone());
            // Also skip temp/old siblings an interrupted export may have left behind.
            if let (Some(parent), Some(base)) = (out_real.parent(), out_real.file_name()) {
                let base = base.to_string_lossy().into_owned();
                if let Ok(rd) = std::fs::read_dir(parent) {
                    for e in rd.flatten() {
                        let name = e.file_name().to_string_lossy().into_owned();
                        if is_exporter_temp_name(&name, &base) {
                            ignore.push(relative_path(&context_real, &parent.join(&name)));
                            skip.push(parent.join(&name));
                        }
                    }
                }
            }
        }
    }
    let vendor_dir = find_vendor_dir(&context_real, options.vendor_dir.as_deref())?;
    if let Some(d) = &vendor_dir {
        ignore.push(d.clone());
        let mut abs = context_real.clone();
        for seg in d.split('/') {
            abs.push(seg);
        }
        skip.push(abs);
    }
    for w in &options.also_writes {
        let real = realpath_loose(&resolve_path(Path::new(w)));
        let rel = relative_path(&context_real, &real);
        if !rel.is_empty() && !rel.starts_with("..") && !rel.starts_with('/') {
            ignore.push(rel);
            skip.push(real);
        }
    }
    let local = LocalContext::open(
        &context_real,
        &LocalContextOptions {
            ignore: Some(ignore),
            sitefileignore: true,
            allow_hardlinks: options.allow_hardlinks,
        },
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
    let mut pins = if options.strict_pins {
        Some(check_pins(&p, lock.as_ref(), base_ref.as_deref())?)
    } else {
        None
    };
    let mut lock = lock;
    // A --target build skips later stages; their FROM image lockfile entries are kept all the same.
    for st in &all_stages[p.stages.len()..] {
        if let (
            StageBase::Image {
                reference,
                local: false,
                lead,
                span,
                ..
            },
            Some(l),
        ) = (&st.base, lock.as_mut())
        {
            l.mark_image_used(&image_lock_key(reference, lead, span)?);
        }
    }
    let lock = Mutex::new(lock);

    let env = ImportEnv {
        local: &local,
        archive_limits: &options.archive_limits,
        keep_os_metadata: options.keep_os_metadata,
        remote: RemoteEnv {
            policy: &options.fetch_policy,
            lock: &lock,
            label: &label,
            warn: &warn,
            frozen: options.frozen_lockfile,
            update: options.update_lock,
            vendor: vendor.as_ref().map(|v| (v, &local)),
            archive_limits: options.archive_limits,
            secrets: &options.secrets,
        },
        memo: options.memo.as_deref(),
        cancel: options.cancel.as_ref(),
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
                pr.reserve(
                    format!("import {}", spec.name),
                    "import",
                    vec![("uri", json!(spec.uri)), ("as", json!(as_kind))],
                    tid,
                )
            })
        })
        .collect();
    // Sitefile imports (AS sitefile) load after the others, one after the other, in Sitefile order.
    let indexed: Vec<(usize, &ImportSpec)> = p
        .imports
        .iter()
        .enumerate()
        .filter(|(_, s)| s.as_kind != Some(ImportKind::Sitefile))
        .collect();
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
    // DRAFTS false drops drafts; then SCHEMA checks run in Sitefile order, on the documents in path order; then SORT BY.
    let mut by_name: HashMap<String, (Resource, String)> = HashMap::new();
    for ((_, s), (res, key)) in indexed.iter().zip(resolved) {
        by_name.insert(s.name.clone(), (without_drafts(s, res)?, key));
    }
    for spec in &p.imports {
        if let (Some(_), Some((res, _))) = (&spec.schema, by_name.get(&spec.name)) {
            check_import_schema(spec, res, &local)?;
        }
    }

    // FROM image stage bases: resolved when their stage starts (registry pull or local store).
    let image_store: std::cell::OnceCell<Store> = std::cell::OnceCell::new();
    let used_images: Mutex<Vec<FromImageUse>> = Mutex::new(Vec::new());
    let image_warn = {
        let w = warn.clone();
        move |m: &str| w(m)
    };
    let from_image_in = |reference: &str,
                         at: Option<&str>,
                         is_local: bool,
                         lead: &str,
                         span: &SourceSpan,
                         unit: Option<&str>|
     -> Result<FromImage> {
        options.check_cancelled()?;
        let store = match image_store.get() {
            Some(s) => s,
            None => {
                let s = options.open_store()?;
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
                network: options.fetch_policy.network.clone(),
                ..Default::default()
            },
            warn: Some(&image_warn),
            span: Some(span.clone()),
            lead: Some(format!("{lead} image")),
            ..ResolveBaseOptions::new(store)
        };
        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let r = resolve_from_image(reference, at, is_local, guard.as_mut(), &o)?;
        used_images
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(FromImageUse {
                name: r.name.clone(),
                digest: r.digest.clone(),
                line: span.line,
                local: is_local,
                import: unit.map(str::to_string),
            });
        Ok(r)
    };
    let from_image =
        |reference: &str, at: Option<&str>, is_local: bool, lead: &str, span: &SourceSpan| {
            from_image_in(reference, at, is_local, lead, span, None)
        };

    let units_env = UnitsEnv {
        remote: env.remote,
        memo: options.memo.as_deref(),
        concurrency: options.concurrency,
        allow_hardlinks: options.allow_hardlinks,
        keep_os_metadata: options.keep_os_metadata,
        strict_pins: options.strict_pins,
        skip,
        temp_dirs,
        record_steps: options.record_steps,
        pins: Mutex::new(Vec::new()),
        plugin_policy: options.plugin_policy,
        tool_policy: options.tool_policy,
        from_image: &from_image_in,
        cancel: options.cancel.as_ref(),
        store_home: options.store_home.clone(),
    };
    let root_id = if options.sitefile.is_none() {
        context_real.to_string_lossy().into_owned()
    } else {
        realpath_loose(&sitefile_path)
            .to_string_lossy()
            .into_owned()
    };
    let root_chain = vec![(root_id, label.clone())];
    let scope = root_scope(options, &label);
    let root = Importer {
        name: None,
        label: &label,
        local: &local,
        scope: &scope,
        chain: &root_chain,
    };
    let full_refs = imported_stage_refs(&all_stages);
    let mut resources: HashMap<String, Resource> = HashMap::new();
    let mut resource_list: Vec<(String, Resource)> = Vec::new();
    // Incremental rebuilds: what each import is (its spec and the digest of what it read).
    let mut resource_keys: HashMap<String, String> = HashMap::new();
    let mut units: Vec<(String, ImportedSitefile)> = Vec::new();
    for (i, spec) in p.imports.iter().enumerate() {
        let (res, key) = match by_name.remove(&spec.name) {
            Some((res, key)) => (sorted(spec, res)?, key),
            None => {
                if let Some(s) = &import_spans[i] {
                    s.start();
                }
                let unit = load_sitefile_import(spec, &root, &full_refs, &units_env)?;
                if let Some(s) = &import_spans[i] {
                    s.end(vec![("kind", json!("tree"))]);
                }
                let key = if options.memo.is_some() {
                    memo_key(&["sitefile", &label, &format!("{spec:?}"), &unit.digest])
                } else {
                    String::new()
                };
                let res = unit.resource();
                units.push((spec.name.clone(), unit));
                (res, key)
            }
        };
        resources.insert(spec.name.clone(), res.clone());
        resource_list.push((spec.name.clone(), res));
        resource_keys.insert(spec.name.clone(), key);
    }
    if let Some(pins) = pins.as_mut() {
        pins.imports
            .extend(units_env.pins.lock().map(|p| p.clone()).unwrap_or_default());
    }
    let memo = options.memo.as_deref();
    // The Sitefile label and its ARG/LET values (sorted: Env is a HashMap).
    let env_key = match memo {
        Some(_) => {
            let mut vars: Vec<_> = p.vars.iter().collect();
            vars.sort_by(|a, b| a.0.cmp(b.0));
            // html LETs are part of it: they are not values, but a template that uses one renders their text.
            let fragments: Vec<(&str, &str)> = p
                .fragments
                .iter()
                .map(|f| (f.name.as_str(), f.text.as_str()))
                .collect();
            memo_key(&[
                "env",
                &label,
                &format!("{vars:?}"),
                &format!("{fragments:?}"),
            ])
        }
        None => String::new(),
    };

    let plugin_warn = warn.clone();
    let partials: Partials = Mutex::new(HashMap::new());
    let mut exec = Exec {
        local: &local,
        resources: &resources,
        stages: HashMap::new(),
        vars: &p.vars,
        fragments: &p.fragments,
        partials: &partials,
        label: &label,
        warn: warn.clone(),
        plugins: {
            let mut h = PluginHost::new(options.plugin_policy, move |m: &str| plugin_warn(m));
            h.set_store_home(options.store_home.clone());
            h.set_reuses_results(options.cache_dir.is_some() || memo.is_some());
            if let Some(node) = &options.node {
                h.set_node(node.clone());
            }
            if let Some(n) = options.on_notice.clone() {
                h.set_notice(move |m: &str| n(m));
            }
            h
        },
        tools: tools.iter().map(|t| (t.name.clone(), t.clone())).collect(),
        tool_policy: options.tool_policy,
        cache: options
            .cache_dir
            .as_ref()
            .map(|d| TransformCache::new(d).with_remote(options.remote_cache.clone())),
        plugin_digests: HashMap::new(),
        stats: TransformStats::default(),
        profiler: prof,
        memo,
        resource_keys,
        env_key,
        stage_keys: HashMap::new(),
        input_key: String::new(),
        renames: BTreeMap::new(),
        recorded: None,
        unrecorded: warn.clone(),
        from_image: &from_image,
        last_renames: None,
        fetch_policy: options.fetch_policy.clone(),
        cancel: options.cancel.as_ref(),
    };
    let mut steps: Option<Vec<Vec<Tree>>> = options.record_steps.then(Vec::new);
    // FINGERPRINT renames: per operation (by line), and per stage (composed, inherited by FROM <stage>).
    let mut renames: Vec<(u32, Vec<(String, String)>)> = Vec::new();
    let mut stage_renames: HashMap<String, BTreeMap<String, String>> = HashMap::new();
    let mut current_renames: BTreeMap<String, String> = BTreeMap::new();
    let run = (|| -> Result<Tree> {
        for spec in &p.plugins {
            let s = begin(
                prof,
                || format!("PLUGIN {}", spec.name),
                "plugin",
                || vec![("path", json!(format!("./{}", spec.path)))],
            );
            register_plugin(spec, &local, &mut exec.plugins, &plugin_permission)?;
            s.end(Vec::new());
        }
        // The stages of imported Sitefiles this build reads (`FROM theme+assets`, `--from=theme+assets`).
        provide_imported_stages(
            &mut units,
            &imported_stage_refs(&p.stages),
            &mut exec.stages,
            &mut exec.stage_keys,
            &mut stage_renames,
            &units_env,
        )?;
        let mut tree = Tree::empty();
        for (i, stage) in p.stages.iter().enumerate() {
            let name = stage.name.clone().unwrap_or_else(|| format!("#{}", i + 1));
            let s = begin(
                prof,
                || format!("stage {name}"),
                "stage",
                || vec![("from", json!(stage_from(stage)))],
            );
            let (start, mut key) = exec.start_stage(stage)?;
            tree = start;
            current_renames = match &stage.base {
                StageBase::Stage { name } => stage_renames.get(name).cloned().unwrap_or_default(),
                _ => BTreeMap::new(),
            };
            let mut record = steps.as_ref().map(|_| vec![tree.clone()]);
            for op in &stage.operations {
                let line = op.span().line;
                let o = begin(
                    prof,
                    || format!("{label}:{line} {}", op.keyword()),
                    "operation",
                    || vec![("line", json!(line))],
                );
                exec.renames.clone_from(&current_renames);
                let (out, k, state) = exec.execute_op(op, tree, &key)?;
                tree = out;
                key = k;
                if let Some(r) = exec.last_renames.take() {
                    current_renames = compose_renames(&current_renames, &r);
                    renames.push((line, r));
                }
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
                stage_renames.insert(name.clone(), current_renames.clone());
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
        return Err(SitefileError::plain(
            E_EXPORT,
            format!(
                "EXPORT path '{}' is a file; export a directory",
                p.export_path
            ),
        ));
    }
    let tree = with_site_rules(tree, &p, &label, &current_renames, &|m: &str| warn(m))?;
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
            options.check_cancelled()?;
            let store = options.open_store()?;
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
                    network: options.fetch_policy.network.clone(),
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
    // A build cancelled after its last check still fails: nothing is saved for it.
    options.check_cancelled()?;
    // Imported Sitefiles' own imports follow, by qualified name (`theme+data`), for the provenance.
    let mut resource_list = resource_list;
    nested_resources(&units, &mut resource_list);

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
        imported: units,
        renames,
    })
}

/// REDIRECT and HEADER lines become `/_redirects` and `/_headers` at the root of the exported directory.
/// A HEADER pattern or REDIRECT target that is the URL of a file FINGERPRINT renamed in the exported stage
/// names its new URL.
fn with_site_rules(
    mut tree: Tree,
    p: &Plan,
    label: &str,
    renames: &BTreeMap<String, String>,
    warn: &dyn Fn(&str),
) -> Result<Tree> {
    let url_below = |path: &str| -> Option<String> {
        if p.export_path == "/" {
            Some(path.to_string())
        } else {
            path.strip_prefix(&format!("{}/", p.export_path))
                .map(|rest| format!("/{rest}"))
        }
    };
    let urls: HashMap<String, String> = renames
        .iter()
        .filter_map(|(a, b)| Some((url_below(a)?, url_below(b)?)))
        .collect();
    let renamed = |s: &str| urls.get(s).cloned().unwrap_or_else(|| s.to_string());
    let to: Vec<String> = p.redirects.iter().map(|r| renamed(&r.to)).collect();
    let patterns: Vec<String> = p.headers.iter().map(|h| renamed(&h.pattern)).collect();
    let redirects = redirects_file(
        p.redirects
            .iter()
            .zip(&to)
            .map(|(r, to)| (r.from.as_str(), to.as_str(), r.status)),
    );
    // FINGERPRINT's Cache-Control lines, after the HEADER lines (see `cache_header_rules`).
    let mut auto: Vec<(String, &str)> = Vec::new();
    let mut header_rules: Vec<(bool, &SourceSpan)> =
        p.headers.iter().map(|h| (h.overwrite, &h.span)).collect();
    let mut header_kind = "HEADER";
    let headers_path = join_virtual(&p.export_path, "_headers");
    if let (Some(fp), false) = (&p.cache_headers, urls.is_empty()) {
        let files: Vec<String> = tree
            .subtree(&p.export_path)
            .entries()
            .map(|(f, _)| f.clone())
            .collect();
        let present: HashSet<&String> = files.iter().collect();
        let hashed: HashSet<String> = urls
            .values()
            .filter(|u| present.contains(u))
            .cloned()
            .collect();
        let rules = cache_header_rules(&files, &hashed);
        if !rules.is_empty() && p.headers.is_empty() && tree.is_file(&headers_path) {
            warn(&format!("{label}:{} FINGERPRINT: {headers_path} already exists (not written by HEADER), so no Cache-Control lines were added for the fingerprinted files; add them to that file, or use HEADER", fp.line));
        } else if !rules.is_empty() {
            auto = rules;
            if p.headers.is_empty() {
                header_rules = vec![(false, fp)];
                header_kind = "FINGERPRINT";
            }
        }
    }
    let headers = headers_file(
        p.headers
            .iter()
            .zip(&patterns)
            .map(|(h, pattern)| (pattern.as_str(), h.name.as_str(), h.value.as_str()))
            .chain(
                auto.iter()
                    .map(|(pattern, value)| (pattern.as_str(), "Cache-Control", *value)),
            ),
    );
    /// (file name, content, each rule's `--overwrite` and span, instruction)
    type SiteFile<'a> = (&'a str, String, Vec<(bool, &'a SourceSpan)>, &'a str);
    let files: [SiteFile; 2] = [
        (
            "_redirects",
            redirects,
            p.redirects.iter().map(|r| (r.overwrite, &r.span)).collect(),
            "REDIRECT",
        ),
        ("_headers", headers, header_rules, header_kind),
    ];
    for (name, text, rules, kind) in files {
        let Some((_, first)) = rules.first() else {
            continue;
        };
        let policy = policy_of(rules.iter().any(|(o, _)| *o));
        let node = make_file(
            text.into_bytes(),
            format!("{label}:{} {kind}", first.line),
            0o644,
        );
        tree = tree.with_files(
            [(join_virtual(&p.export_path, name), node)],
            policy,
            Some(first),
        )?;
    }
    Ok(tree)
}

// ---------------------------------------------------------------- export

/// Overrides for where and how the export is written.
#[derive(Debug, Clone, Default)]
pub struct ExportTarget {
    /// `--experimental-dictionary`: the previous build's files; the export gets `.dcz` deltas
    /// against them and `Use-As-Dictionary` headers (crate::dictionary).
    pub dictionary_from: Option<Tree>,
    /// Overrides the Sitefile's EXPORT format.
    pub format: Option<ExportFormat>,
    /// Overrides the Sitefile's EXPORT ... BASE (wbn).
    pub base_url: Option<String>,
    /// oci: root directory inside the layer (overrides EXPORT ... AT).
    pub oci_root: Option<String>,
    /// oci: site layer encoding (overrides EXPORT ... COMPRESSION; default gzip).
    pub oci_compression: Option<OciCompression>,
    /// oci: split the site's files over this many layers (`--oci-layers`, default 1).
    pub oci_layers: Option<usize>,
    /// wbn: `gzip` wraps the bundle in gzip (overrides EXPORT ... COMPRESSION; default none).
    pub wbn_compression: Option<OciCompression>,
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
    /// car: the root CID (CIDv1, base32).
    pub root_cid: Option<String>,
    /// Watch mode: set when the directory export applied only the changes (export::incremental).
    pub incremental: Option<crate::export::incremental::Applied>,
    /// `--experimental-dictionary`: the paths that got a `.dcz` delta.
    pub dictionary_deltas: Option<Vec<String>>,
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
    let facts = build_facts(
        result,
        &options.context_dir,
        options.sitefile.as_deref(),
        options.label.as_deref(),
        &options.build_args,
        options.platform.as_deref(),
    )?;
    let cfg = crate::export::oci::parse_config(config_bytes)
        .map_err(|e| SitefileError::plain(E_STORE, format!("image config: {e}")))?;
    let title = cfg
        .labels
        .get(crate::export::oci::ann::TITLE)
        .map(String::as_str);
    let subject = AttestSubject {
        digest: manifest_digest,
        size: manifest_bytes.len() as u64,
        files: &result.output,
        root,
        title,
    };
    Ok(make_attestations(
        &subject,
        &facts,
        request,
        source_date_epoch()?,
    ))
}

/// The Sitefile's `EXPORT ... COMPRESSION` for an export in `format`: it belongs to the EXPORT's own
/// format, so a `--format` override to another format does not inherit it (None: that format's default).
pub fn sitefile_compression(plan: &Plan, format: ExportFormat) -> Option<OciCompression> {
    if plan.export_format == format {
        plan.export_compression
    } else {
        None
    }
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
        root: root
            .map(str::to_string)
            .or_else(|| result.plan.export_root.clone()),
        platform: platform.map(str::to_string),
        epoch: source_date_epoch()?,
        title: meta.title,
        source: meta.source,
        revision: meta.revision,
        labels: meta.labels,
        base: result.base.clone(),
        compression: compression
            .or(sitefile_compression(&result.plan, ExportFormat::Oci))
            .unwrap_or_default(),
        layers: None,
    })
}

/// How [`store_build_as_image`] builds the image: what `build -t` takes from
/// its flags. `ImageInfo::new(context_dir)` is a plain `build -t` (no labels,
/// no attestations, the Sitefile's root and compression).
#[derive(Debug, Clone, Default)]
pub struct ImageInfo {
    /// The build context (the image title and, with `git_labels`, its Git labels).
    pub context_dir: String,
    /// `--oci-root`: overrides the Sitefile's EXPORT path inside the image.
    pub oci_root: Option<String>,
    /// `--oci-compression` / `--compression`: overrides `EXPORT … AS oci COMPRESSION`.
    pub oci_compression: Option<OciCompression>,
    /// `--oci-layers`: split the site's files over this many layers.
    pub oci_layers: Option<usize>,
    /// `--platform`: `os/arch` of the image.
    pub platform: Option<String>,
    /// `--label key=value`.
    pub labels: std::collections::BTreeMap<String, String>,
    /// `--git-labels`.
    pub git_labels: bool,
    /// `--provenance` / `--sbom`: attach these attestations to the stored image.
    pub attest: crate::export::attest::AttestRequest,
}

impl ImageInfo {
    pub fn new(context_dir: impl Into<String>) -> Self {
        ImageInfo {
            context_dir: context_dir.into(),
            ..Default::default()
        }
    }
}

/// What [`store_build_as_image`] stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredImage {
    /// The image ID (`sha256:…`, the manifest digest).
    pub id: String,
    /// Each tag as it was recorded (`name:tag`), in the order given.
    pub tags: Vec<String>,
}

/// Validate `-t` values before building: local `name[:tag]` only, no digest.
pub fn validate_image_tags(tags: &[String]) -> Result<()> {
    for t in tags {
        if crate::refs::parse_local_ref(t)?.digest.is_some() {
            return Err(SitefileError::plain(
                E_USAGE,
                format!("-t '{t}': a tag cannot include a digest"),
            ));
        }
    }
    Ok(())
}

/// `build -t`: store a build's exported tree as an OCI image in `store`, attach
/// the requested attestations, and point each tag at it. No tags stores an
/// untagged (`<none>`) image. Tags are validated first ([`validate_image_tags`]),
/// and a cancelled build ([`BuildOptions::cancel`]) stores nothing (`E_CANCELLED`).
/// Prints nothing; the CLI prints `Tagged <tag> <short id> (<n> files)` per tag.
pub fn store_build_as_image(
    store: &Store,
    result: &BuildResult,
    tags: &[String],
    info: &ImageInfo,
    build_options: &BuildOptions,
) -> Result<StoredImage> {
    validate_image_tags(tags)?;
    build_options.check_cancelled()?;
    let mut opts = oci_options(
        result,
        &info.context_dir,
        info.oci_root.as_deref(),
        info.oci_compression,
        info.platform.as_deref(),
        &info.labels,
        info.git_labels,
    )?;
    opts.layers = info.oci_layers;
    let added = store.add_image(&result.output, &opts)?;
    if info.attest.any() {
        let manifest_bytes = store.read_blob(&added.id, None)?;
        let d = store.details(&added.id)?;
        let config_bytes = store.read_blob(&d.manifest.config.digest, None)?;
        let referrers = image_attestations(
            result,
            &added.id,
            &manifest_bytes,
            &config_bytes,
            &d.root,
            build_options,
            info.attest,
        )?;
        store.put_referrers(&added.id, &referrers)?;
    }
    let mut keys = Vec::new();
    for t in tags {
        keys.push(store.tag(&added.id, &crate::refs::parse_local_ref(t)?)?);
    }
    Ok(StoredImage {
        id: added.id,
        tags: keys,
    })
}

/// Build, then write the exported tree to `out_dir`: a directory for the
/// `directory` format, a single file for zip, tar, tgz and wbn; oci writes an
/// OCI archive for `*.tar` and an image layout directory otherwise.
pub fn build_to_directory(
    options: &BuildOptions,
    out_dir: &str,
    target: &ExportTarget,
) -> Result<ExportedBuild> {
    let result = build(options)?;
    let format = target.format.unwrap_or(result.plan.export_format);
    let span = begin(
        options.profiler.as_deref(),
        || "export".into(),
        "export",
        || vec![("format", json!(format.as_str()))],
    );
    let r = export_build(result, format, options, out_dir, target);
    span.end(Vec::new());
    r
}

fn export_build(
    mut result: BuildResult,
    format: ExportFormat,
    options: &BuildOptions,
    out_dir: &str,
    target: &ExportTarget,
) -> Result<ExportedBuild> {
    let mut deltas = None;
    if let Some(previous) = &target.dictionary_from {
        let (tree, d) = crate::dictionary::add_dictionary_deltas(&result.output, previous)?;
        result.output = tree;
        deltas = Some(d);
    }
    let mut exported = export_build_inner(result, format, options, out_dir, target)?;
    exported.dictionary_deltas = deltas;
    Ok(exported)
}

fn export_build_inner(
    result: BuildResult,
    format: ExportFormat,
    options: &BuildOptions,
    out_dir: &str,
    target: &ExportTarget,
) -> Result<ExportedBuild> {
    // Watch mode: a directory export writes only what changed since the session's last one.
    let export_state = options
        .memo
        .as_ref()
        .map(|m| lock_memo(m).export_state.clone());
    if format != ExportFormat::Directory {
        if let Some(s) = &export_state {
            crate::export::incremental::clear_shared(s);
        }
    }
    let export_options = ExportOptions::new(&options.context_dir).force(target.force);
    let out = Path::new(out_dir);
    if format == ExportFormat::Oci {
        let mut opts = oci_options(
            &result,
            &options.context_dir,
            target.oci_root.as_deref(),
            target.oci_compression,
            options.platform.as_deref(),
            &target.image_labels,
            target.git_labels,
        )?;
        opts.layers = target.oci_layers;
        let img = build_oci_image(&result.output, &opts)?;
        // An image built on a BASE carries the base's layers: copy them in so the export is self-contained.
        let base_blobs = match &result.base {
            Some(_) => read_layer_blobs(&options.open_store()?, &img.base_layers)?,
            None => Vec::new(),
        };
        let ref_name = target.oci_ref_name.as_deref();
        let referrers = image_attestations(
            &result,
            &img.manifest_digest,
            &img.manifest_bytes,
            &img.config_bytes,
            &img.root,
            options,
            target.attest,
        )?;
        if out_dir.to_lowercase().ends_with(".tar") {
            let data = oci_archive(
                img.parts(),
                ref_name,
                source_date_epoch()?,
                &base_blobs,
                &referrers,
            )?;
            let written = export_file(&data, out, &export_options)?;
            let digest = sha256(&data);
            return Ok(ExportedBuild {
                result,
                format,
                out_path: written.out_file,
                bytes: Some(written.bytes),
                file_digest: Some(digest),
                manifest_digest: Some(img.manifest_digest.clone()),
                root_cid: None,
                incremental: None,
                dictionary_deltas: None,
            });
        }
        let written = export_directory(
            &layout_tree(img.parts(), ref_name, &base_blobs, &referrers)?,
            out,
            &export_options,
        )?;
        return Ok(ExportedBuild {
            result,
            format,
            out_path: written.out_dir,
            bytes: None,
            file_digest: None,
            manifest_digest: Some(img.manifest_digest.clone()),
            root_cid: None,
            incremental: None,
            dictionary_deltas: None,
        });
    }
    if !is_archive_format(format) {
        let written = export_directory(
            &result.output,
            out,
            &export_options.incremental(export_state),
        )?;
        return Ok(ExportedBuild {
            result,
            format,
            out_path: written.out_dir,
            bytes: None,
            file_digest: None,
            manifest_digest: None,
            root_cid: None,
            incremental: written.incremental,
            dictionary_deltas: None,
        });
    }
    if format == ExportFormat::Car {
        let (data, root) = crate::export::car::encode_car(&result.output)?;
        let written = export_file(&data, out, &export_options)?;
        return Ok(ExportedBuild {
            result,
            format,
            out_path: written.out_file,
            bytes: Some(written.bytes),
            file_digest: Some(sha256(&data)),
            manifest_digest: None,
            root_cid: Some(root),
            incremental: None,
            dictionary_deltas: None,
        });
    }
    let data = encode_tree(
        &result.output,
        format,
        &EncodeOptions {
            writer: WriterOptions {
                epoch: source_date_epoch()?,
                dirs: Vec::new(),
            },
            base_url: target
                .base_url
                .clone()
                .or_else(|| result.plan.export_base.clone()),
            compression: if format == ExportFormat::Wbn {
                target
                    .wbn_compression
                    .or(sitefile_compression(&result.plan, ExportFormat::Wbn))
            } else {
                None
            },
        },
    )?;
    let written = export_file(&data, out, &export_options)?;
    let digest = sha256(&data);
    Ok(ExportedBuild {
        result,
        format,
        out_path: written.out_file,
        bytes: Some(written.bytes),
        file_digest: Some(digest),
        manifest_digest: None,
        root_cid: None,
        incremental: None,
        dictionary_deltas: None,
    })
}

// ---------------------------------------------------------------- imports

struct ImportEnv<'a> {
    local: &'a LocalContext,
    archive_limits: &'a ArchiveLimits,
    keep_os_metadata: bool,
    remote: RemoteEnv<'a>,
    memo: Option<&'a Mutex<BuildMemo>>,
    cancel: Option<&'a CancelToken>,
}

fn check_cancel(cancel: Option<&CancelToken>) -> Result<()> {
    match cancel {
        Some(c) => c.check(),
        None => Ok(()),
    }
}

/// What fetching a remote IMPORT needs: the network policy and the lockfile rules (and, for builds,
/// the vendor directory with the context reader for its blobs).
#[derive(Clone, Copy)]
pub struct RemoteEnv<'a> {
    pub policy: &'a FetchPolicy,
    pub lock: &'a Mutex<Option<Lockfile>>,
    pub label: &'a str,
    pub warn: &'a Callback,
    pub frozen: bool,
    pub update: bool,
    /// Vendored copies (`sitefile vendor`), preferred over the network.
    pub vendor: Option<(&'a Vendor, &'a LocalContext)>,
    /// Limits for Git trees (file count, total size).
    pub archive_limits: ArchiveLimits,
    /// Build secrets by id (`--secret`), sent as the headers of IMPORT … SECRET clauses.
    pub secrets: &'a Secrets,
}

/// The canonical bytes `sitefile vendor` stores for a Git selection: a file's bytes, or a tree as a deterministic tar.
pub fn git_selection_bytes(selection: &GitSelection) -> Result<Vec<u8>> {
    match selection {
        GitSelection::File { bytes, .. } => Ok(bytes.to_vec()),
        GitSelection::Tree(files) => {
            let tree = Tree::from_files(
                files
                    .iter()
                    .map(|(p, b, m)| (p.clone(), make_file(b.clone(), "vendor", *m))),
            )?;
            crate::export::archive_writers::write_tar(
                &tree,
                &WriterOptions {
                    epoch: Some(0),
                    dirs: Vec::new(),
                },
            )
        }
    }
}

/// The content of a Git IMPORT, checked against the lockfile (and recorded there): (commit, selection).
/// See the TypeScript `fetchGitImport`.
pub fn fetch_git_import(
    spec: &ImportSpec,
    loc: &GitLocation,
    env: &RemoteEnv<'_>,
) -> Result<(String, GitSelection)> {
    let url = &loc.url;
    let span = Some(&spec.span);
    let entry = env
        .lock
        .lock()
        .ok()
        .and_then(|l| l.as_ref().and_then(|l| l.get_git(url).cloned()));
    if env.frozen && entry.is_none() {
        return Err(SitefileError::at(
            E_LOCK_MISSING,
            format!(
                "IMPORT {}: '{url}' is not in the lockfile (--frozen-lockfile)",
                spec.name
            ),
            &spec.span,
        ));
    }
    if let (Some(e), Some(c)) = (&entry, &loc.commit) {
        if &e.commit != c {
            return Err(SitefileError::at(
                E_LOCK_MISMATCH,
                format!("IMPORT {}: '{url}' is locked to commit {}; rerun with --update-lock to accept {c}", spec.name, e.commit),
                &spec.span,
            ));
        }
    }
    let mut want: Option<String> = loc.commit.clone().or_else(|| {
        entry
            .as_ref()
            .filter(|_| !env.update)
            .map(|e| e.commit.clone())
    });
    let vendored = env.vendor.and_then(|(v, _)| v.git(url));
    let use_vendored = match (vendored, &want) {
        (Some(_), None) => !env.update,
        (Some(v), Some(w)) => &v.commit == w,
        (None, _) => false,
    };
    let selection = if let (true, Some((v, local)), Some(ve)) = (use_vendored, env.vendor, vendored)
    {
        want = Some(ve.commit.clone());
        let basename = loc.path.rsplit('/').next().unwrap_or("");
        v.read_git(local, url, ve, basename, &env.archive_limits, span)?
    } else {
        if let Some((v, _)) = env.vendor {
            let at = if vendored.is_some() {
                format!(" at commit {}", want.as_deref().unwrap_or("the latest"))
            } else {
                String::new()
            };
            (env.warn)(&format!(
                "{}:{}: IMPORT {}: '{url}' is not vendored in {}{at}; fetching it (run 'sitefile vendor' to add it)",
                env.label, spec.span.line, spec.name, v.dir
            ));
        }
        let origin_headers = secret_headers(
            &spec.name,
            &spec.secrets,
            &spec.span,
            env.secrets,
            Some((&loc.repo, &env.policy.network)),
        )?;
        let transport = GitTransport {
            origin_headers: &origin_headers,
            ..GitTransport::new(env.policy, span)
        };
        let objs = fetch_git_commit(
            &loc.repo,
            &loc.git_ref,
            want.as_deref(),
            transport,
            &env.archive_limits,
        )?;
        want = Some(objs.commit.clone());
        select_git_path(&objs, &loc.repo, &loc.path, &env.archive_limits, span)?
    };
    let want = want.unwrap_or_default();
    if loc.commit.is_none() && entry.is_none() {
        (env.warn)(&format!(
            "{}:{}: IMPORT {} is not pinned; pin commit {want} (#{want}{}) for a reproducible build",
            env.label,
            spec.span.line,
            spec.name,
            if loc.path.is_empty() { String::new() } else { format!(":{}", loc.path) }
        ));
    }
    if let Ok(mut l) = env.lock.lock() {
        if let Some(l) = l.as_mut() {
            l.record_git(
                url,
                GitLockEntry {
                    commit: want.clone(),
                },
            );
        }
    }
    Ok((want, selection))
}

/// The bytes of a remote IMPORT and its final URL, checked against its INTEGRITY and the lockfile (and
/// recorded there): read from the vendor directory when it has the URL, otherwise fetched.
pub fn fetch_remote_import(
    spec: &ImportSpec,
    url: &str,
    env: &RemoteEnv<'_>,
) -> Result<(Vec<u8>, String)> {
    let span = Some(&spec.span);
    let entry = env
        .lock
        .lock()
        .ok()
        .and_then(|l| l.as_ref().and_then(|l| l.get(url).cloned()));
    if env.frozen && entry.is_none() {
        return Err(SitefileError::at(
            E_LOCK_MISSING,
            format!(
                "IMPORT {}: '{url}' is not in the lockfile (--frozen-lockfile)",
                spec.name
            ),
            &spec.span,
        ));
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
            let origin_headers = secret_headers(
                &spec.name,
                &spec.secrets,
                &spec.span,
                env.secrets,
                Some((url, &env.policy.network)),
            )?;
            let f = fetch_remote_with(
                url,
                env.policy,
                span,
                &FetchRequest {
                    origin_headers,
                    ..FetchRequest::default()
                },
            )?;
            (f.bytes, f.final_url)
        }
    };
    let digest = sha256(&bytes);
    if let Some(i) = &spec.integrity {
        verify_integrity(&bytes, i, url, span)?;
    } else if let Some(e) = entry
        .as_ref()
        .filter(|e| !env.update && e.integrity != digest)
    {
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
        return Err(SitefileError::at(
            E_LOCK_MISMATCH,
            format!(
                "IMPORT {}: '{url}' does not match the lockfile (--frozen-lockfile)",
                spec.name
            ),
            &spec.span,
        ));
    }
    if let Ok(mut l) = env.lock.lock() {
        if let Some(l) = l.as_mut() {
            l.record(
                url,
                LockEntry {
                    integrity: digest,
                    size: bytes.len() as u64,
                    resolved: final_url.clone(),
                },
            );
        }
    }
    Ok((bytes, final_url))
}

/// Run `f` over `items` with at most `limit` at once; results in input order.
/// The first failure (by position) is returned.
fn map_limit<T: Sync, R: Send>(
    items: &[T],
    limit: usize,
    f: impl Fn(&T) -> Result<R> + Sync,
) -> Result<Vec<R>> {
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
    SitefileError::at(
        E_VALIDATION,
        format!(
            "IMPORT {}: INTEGRITY applies to files, not directories",
            spec.name
        ),
        &spec.span,
    )
}

/// `path.posix.basename(new URL(url).pathname)`.
fn url_basename(url: &str) -> String {
    let after = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let path = match after.find('/') {
        Some(i) => &after[i..],
        None => "/",
    };
    let path = path.split(['?', '#']).next().unwrap_or("");
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string()
}

/// Read an import. The key identifies the result for incremental rebuilds: the
/// import's spec (with its Sitefile position) and the digest of the bytes or tree
/// it read. With a memo, conversions (Markdown, data, archives) whose key is
/// unchanged are reused; a collection's documents are memoized one by one.
fn resolve_import(spec: &ImportSpec, env: &ImportEnv<'_>) -> Result<(Resource, String)> {
    check_cancel(env.cancel)?;
    let origin = format!(
        "{}:{} IMPORT {}",
        env.remote.label, spec.span.line, spec.name
    );
    let span = Some(&spec.span);
    let id = if env.memo.is_some() {
        format!("{spec:?}")
    } else {
        String::new()
    };
    let key_of = |raw: &str| {
        if env.memo.is_some() {
            memo_key(&["import", env.remote.label, &id, raw])
        } else {
            String::new()
        }
    };
    // `compute` gets the per-document memo (children of this import's entry).
    let convert =
        |key: &str, compute: &dyn Fn(Option<DocMemo>) -> Result<Resource>| -> Result<Resource> {
            let Some(memo) = env.memo else {
                return compute(None);
            };
            memoize(memo, key, None, |children| {
                let docs = |parts: &[&str],
                            make: &mut dyn FnMut() -> Result<MarkdownDocument>|
                 -> Result<MarkdownDocument> {
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
                        let Resource::Tree { tree, provenance } =
                            env.local.read_tree(rel, &origin, span)?
                        else {
                            unreachable!()
                        };
                        let key = key_of(&provenance.digest);
                        let res = convert(&key, &|docs| {
                            let list = markdown_collection(
                                &tree,
                                &trim_trailing_slashes(&spec.uri),
                                span,
                                docs,
                            )?;
                            Ok(Resource::Value {
                                format: ValueFormat::Markdown,
                                value: Value::Array(list.into_iter().map(Value::Object).collect()),
                                provenance: provenance.clone(),
                            })
                        })?;
                        return Ok((res, key));
                    }
                    Some(k @ (ImportKind::Json | ImportKind::Yaml)) => {
                        if spec.integrity.is_some() {
                            return Err(integrity_on_dir(spec));
                        }
                        let Resource::Tree { tree, provenance } =
                            env.local.read_tree(rel, &origin, span)?
                        else {
                            unreachable!()
                        };
                        let (kind, format) = if k == ImportKind::Json {
                            (DataKind::Json, ValueFormat::Json)
                        } else {
                            (DataKind::Yaml, ValueFormat::Yaml)
                        };
                        let key = key_of(&provenance.digest);
                        let res = convert(&key, &|docs| {
                            let list = data_collection(
                                &tree,
                                kind,
                                &trim_trailing_slashes(&spec.uri),
                                span,
                                docs,
                            )?;
                            Ok(Resource::Value {
                                format,
                                value: Value::Array(list.into_iter().map(Value::Object).collect()),
                                provenance: provenance.clone(),
                            })
                        })?;
                        return Ok((res, key));
                    }
                    Some(k) if k != ImportKind::Tree => {
                        return Err(SitefileError::at(
                            E_TYPE,
                            format!(
                                "IMPORT {}: '{}' is a directory and cannot be imported AS {}",
                                spec.name,
                                spec.uri,
                                k.as_str()
                            ),
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
            let Resource::File {
                bytes,
                mode,
                basename,
                provenance,
            } = &file
            else {
                unreachable!()
            };
            if let Some(i) = &spec.integrity {
                verify_integrity(bytes, i, &spec.uri, span)?;
            }
            let key = key_of(&format!("{}\0{mode}\0{basename}", provenance.digest));
            match spec.as_kind {
                None | Some(ImportKind::File) => Ok((file, key)),
                Some(kind) => {
                    let info = BytesInfo {
                        uri: spec.uri.clone(),
                        resolved_url: None,
                        basename: basename.clone(),
                        mode: Some(*mode),
                    };
                    let res = convert(&key, &|_| {
                        bytes_to_resource(bytes.clone(), kind, &info, span, env.archive_limits)
                            .and_then(|r| drop_os_metadata(r, spec, env))
                    })?;
                    Ok((res, key))
                }
            }
        }
        ImportLocation::Git(loc) => {
            let (commit, selection) = fetch_git_import(spec, loc, &env.remote)?;
            let resolved_url = pinned_git_url(loc, &commit);
            match selection {
                GitSelection::File {
                    bytes,
                    mode,
                    basename,
                } => {
                    let digest = sha256(&bytes);
                    let key = key_of(&format!("{commit}\0{digest}\0{mode}"));
                    let info = BytesInfo {
                        uri: spec.uri.clone(),
                        resolved_url: Some(resolved_url),
                        basename,
                        mode: Some(mode),
                    };
                    let kind = spec.as_kind.unwrap_or(ImportKind::File);
                    let res = convert(&key, &|_| {
                        bytes_to_resource(bytes.to_vec(), kind, &info, span, env.archive_limits)
                            .and_then(|r| drop_os_metadata(r, spec, env))
                    })?;
                    Ok((with_commit(res, &commit), key))
                }
                GitSelection::Tree(files) => {
                    let tree = Tree::from_files(
                        files
                            .into_iter()
                            .map(|(p, b, m)| (p, make_file(b, origin.clone(), m))),
                    )?;
                    let provenance = crate::resources::types::Provenance {
                        uri: spec.uri.clone(),
                        resolved_url: Some(resolved_url),
                        digest: tree.digest(),
                        git_commit: Some(commit.clone()),
                    };
                    let key = key_of(&format!("{commit}\0{}", provenance.digest));
                    let base = trim_trailing_slashes(&spec.uri);
                    match spec.as_kind {
                        Some(ImportKind::Markdown) => {
                            let res = convert(&key, &|docs| {
                                let list = markdown_collection(&tree, &base, span, docs)?;
                                Ok(Resource::Value {
                                    format: ValueFormat::Markdown,
                                    value: Value::Array(
                                        list.into_iter().map(Value::Object).collect(),
                                    ),
                                    provenance: provenance.clone(),
                                })
                            })?;
                            Ok((res, key))
                        }
                        Some(k @ (ImportKind::Json | ImportKind::Yaml)) => {
                            let (kind, format) = if k == ImportKind::Json {
                                (DataKind::Json, ValueFormat::Json)
                            } else {
                                (DataKind::Yaml, ValueFormat::Yaml)
                            };
                            let res = convert(&key, &|docs| {
                                let list = data_collection(&tree, kind, &base, span, docs)?;
                                Ok(Resource::Value {
                                    format,
                                    value: Value::Array(
                                        list.into_iter().map(Value::Object).collect(),
                                    ),
                                    provenance: provenance.clone(),
                                })
                            })?;
                            Ok((res, key))
                        }
                        Some(k) if k != ImportKind::Tree => Err(SitefileError::at(
                            E_TYPE,
                            format!(
                                "IMPORT {}: '{}' is a directory and cannot be imported AS {}",
                                spec.name,
                                spec.uri,
                                k.as_str()
                            ),
                            &spec.span,
                        )),
                        _ => Ok((Resource::Tree { tree, provenance }, key)),
                    }
                }
            }
        }
        ImportLocation::Remote { url } => {
            let kind = spec.as_kind.unwrap_or(ImportKind::File);
            let (bytes, final_url) = fetch_remote_import(spec, url, &env.remote)?;
            let base = url_basename(&final_url);
            let basename = if base.is_empty() {
                spec.name.clone()
            } else {
                base
            };
            let key = key_of(&format!("{}\0{final_url}", sha256(&bytes)));
            let info = BytesInfo {
                uri: url.clone(),
                resolved_url: Some(final_url),
                basename,
                mode: None,
            };
            let res = convert(&key, &|_| {
                bytes_to_resource(bytes.clone(), kind, &info, span, env.archive_limits)
                    .and_then(|r| drop_os_metadata(r, spec, env))
            })?;
            Ok((res, key))
        }
    }
}

/// An archive imported AS tree without its OS metadata files (unless `--keep-os-metadata`), with a warning.
fn drop_os_metadata(res: Resource, spec: &ImportSpec, env: &ImportEnv) -> Result<Resource> {
    if env.keep_os_metadata {
        return Ok(res);
    }
    match res {
        Resource::Tree { tree, provenance } => {
            let (tree, skipped) = without_os_metadata(tree)?;
            if skipped > 0 {
                (env.remote.warn)(&os_metadata_warning(
                    &format!("IMPORT {}", spec.name),
                    skipped,
                ));
            }
            Ok(Resource::Tree { tree, provenance })
        }
        other => Ok(other),
    }
}

fn with_commit(res: Resource, commit: &str) -> Resource {
    match res {
        Resource::File {
            bytes,
            mode,
            basename,
            mut provenance,
        } => {
            provenance.git_commit = Some(commit.to_string());
            Resource::File {
                bytes,
                mode,
                basename,
                provenance,
            }
        }
        Resource::Tree {
            tree,
            mut provenance,
        } => {
            provenance.git_commit = Some(commit.to_string());
            Resource::Tree { tree, provenance }
        }
        Resource::Value {
            format,
            value,
            mut provenance,
        } => {
            provenance.git_commit = Some(commit.to_string());
            Resource::Value {
                format,
                value,
                provenance,
            }
        }
    }
}

/// `IMPORT … DRAFTS false`: leave out the elements whose `draft` is `true` (or the text `true`, as in a CSV cell).
fn without_drafts(spec: &ImportSpec, res: Resource) -> Result<Resource> {
    let Some(keep) = spec.drafts else {
        return Ok(res);
    };
    let Resource::Value {
        format,
        value,
        provenance,
    } = res
    else {
        return Ok(res);
    };
    let Value::Array(items) = value else {
        return Err(SitefileError::at(
            E_TYPE,
            format!("IMPORT {}: DRAFTS needs a list (a directory of documents, a CSV file or a JSON/YAML array), not a single value", spec.name),
            &spec.span,
        ));
    };
    let items = if keep {
        items
    } else {
        items
            .into_iter()
            .filter(|d| {
                !matches!(d.get("draft"), Some(Value::Bool(true)))
                    && d.get("draft").and_then(Value::as_str) != Some("true")
            })
            .collect()
    };
    Ok(Resource::Value {
        format,
        value: Value::Array(items),
        provenance,
    })
}

/// `IMPORT … SCHEMA <path>`: read and compile the schema file, then check the import's documents.
fn check_import_schema(spec: &ImportSpec, res: &Resource, local: &LocalContext) -> Result<()> {
    let Some(schema) = &spec.schema else {
        return Ok(());
    };
    let what = format!("IMPORT {}", spec.name);
    let Resource::Value { format, value, .. } = res else {
        return Err(SitefileError::at(
            E_TYPE,
            format!(
                "{what}: SCHEMA checks documents, but the import is a {}",
                res.kind()
            ),
            &spec.span,
        ));
    };
    let Resource::File { bytes, .. } = local.read_file(&schema.path, Some(&schema.span))? else {
        unreachable!("read_file returns a file")
    };
    let text = decode_utf8(
        &bytes,
        &format!("schema '{}'", schema.uri),
        Some(&schema.span),
    )?;
    let parsed = if schema.path.to_ascii_lowercase().ends_with(".json") {
        serde_json::from_str::<Value>(&text).map_err(|e| {
            SitefileError::at(
                E_SCHEMA,
                format!("{what}: schema '{}' is not valid JSON: {e}", schema.uri),
                &schema.span,
            )
        })?
    } else {
        crate::yaml::parse_yaml(&text, &schema.uri)?
    };
    let compiled = crate::schema::compile_schema(&parsed, &schema.uri, &what, &schema.span)?;
    crate::schema::check_schema(
        value,
        &compiled,
        &what,
        *format == ValueFormat::Csv,
        &spec.span,
    )
}

/// Apply an import's SORT BY to a list value.
fn sorted(spec: &ImportSpec, res: Resource) -> Result<Resource> {
    let Some(sort) = &spec.sort else {
        return Ok(res);
    };
    let Resource::Value {
        format,
        value,
        provenance,
    } = res
    else {
        return Ok(res);
    };
    let Value::Array(items) = value else {
        return Err(SitefileError::at(
            E_TYPE,
            format!("IMPORT {}: SORT BY needs a list (a directory of documents, a CSV file or a JSON/YAML array), not a single value", spec.name),
            &spec.span,
        ));
    };
    if !items.iter().all(Value::is_object) {
        return Err(SitefileError::at(
            E_TYPE,
            format!(
                "IMPORT {}: SORT BY needs a list of objects with a '{}' field",
                spec.name, sort.key
            ),
            &spec.span,
        ));
    }
    Ok(Resource::Value {
        format,
        value: Value::Array(sort_documents(items, &sort.key, sort.descending)),
        provenance,
    })
}

// ---------------------------------------------------------------- Sitefile imports (IMPORT … AS sitefile)

/// How deep Sitefile imports may nest (an imported Sitefile importing another counts one more level).
pub const MAX_SITEFILE_IMPORT_DEPTH: usize = 8;

/// Where a Sitefile lives, for the labels of the Sitefiles it imports: `<prefix><dir>/Sitefile`. Local
/// Sitefiles have no prefix and `dir` relative to the working directory; a Git checkout has
/// `git+https://host/repo.git#<commit>:` and the directory inside the repository.
#[derive(Debug, Clone)]
pub struct LabelScope {
    pub prefix: String,
    pub dir: String,
}

/// The label an imported Sitefile's messages and file origins use.
pub fn sitefile_label(scope: &LabelScope) -> String {
    format!("{}{}", scope.prefix, join_path(&scope.dir, "Sitefile"))
}

/// `path.posix.dirname`.
fn posix_dirname(p: &str) -> String {
    match p.rfind('/') {
        Some(0) => "/".into(),
        Some(i) => p[..i].to_string(),
        None => ".".into(),
    }
}

/// The label scope of the build's own Sitefile.
fn root_scope(options: &BuildOptions, label: &str) -> LabelScope {
    if options.sitefile.is_none() {
        return LabelScope {
            prefix: String::new(),
            dir: posix_dirname(label),
        };
    }
    let rel = relative_path(&cwd(), &resolve_path(Path::new(&options.context_dir)));
    LabelScope {
        prefix: String::new(),
        dir: if rel.is_empty() { ".".into() } else { rel },
    }
}

/// The ARG values an `IMPORT … AS sitefile` passes, checked against the imported Sitefile's ARGs.
pub fn imported_args(spec: &ImportSpec, program: &Program) -> Result<BuildArgs> {
    let declared: Vec<(&str, bool)> = program
        .instructions
        .iter()
        .filter_map(|i| match i {
            Instruction::Arg {
                name,
                default_value,
                ..
            } => Some((name.as_str(), default_value.is_some())),
            _ => None,
        })
        .collect();
    let mut args = BuildArgs::new();
    for a in &spec.args {
        if !declared.iter().any(|(n, _)| *n == a.name) {
            return Err(SitefileError::at(
                E_UNDEFINED,
                format!(
                    "IMPORT {}: {} has no ARG {}",
                    spec.name, program.file, a.name
                ),
                &a.span,
            ));
        }
        args.set(a.name.clone(), a.value.clone());
    }
    for (name, has_default) in &declared {
        if !has_default && args.get(name).is_none() {
            return Err(SitefileError::at(
                E_MISSING_ARG,
                format!(
                    "IMPORT {}: ARG {name} of {} has no default; set it with ARG {name}=<value>",
                    spec.name, program.file
                ),
                &spec.span,
            ));
        }
    }
    Ok(args)
}

/// Parse, check and plan an imported Sitefile: no PLUGIN or TOOL, the IMPORT's ARG clauses, and the
/// stages `refs` (references from the importing Sitefile) must exist.
pub fn plan_imported_sitefile(
    spec: &ImportSpec,
    source: &str,
    label: &str,
    refs: &[ImportedStageRef],
) -> Result<Plan> {
    let program = parse_program(source, label)?;
    for ins in &program.instructions {
        if let Instruction::Plugin { name, span, .. } | Instruction::Tool { name, span } = ins {
            return Err(SitefileError::at(
                E_UNSUPPORTED,
                format!(
                    "{} {name}: a Sitefile imported AS sitefile cannot run plugins or tools",
                    ins.keyword()
                ),
                span,
            ));
        }
    }
    let args = imported_args(spec, &program)?;
    let import = spec.name.clone();
    let p = plan_with(&program, &args, &|n| format!("IMPORT {import} ARG {n}"))?;
    let names: Vec<&str> = p.stages.iter().filter_map(|s| s.name.as_deref()).collect();
    for r in refs {
        if r.import != spec.name || names.contains(&r.stage.as_str()) {
            continue;
        }
        let which = if names.is_empty() {
            "it has no named stages".to_string()
        } else {
            format!("stages: {}", names.join(", "))
        };
        return Err(SitefileError::at(
            E_UNDEFINED,
            format!("{}: {label} has no stage '{}' ({which})", r.name, r.stage),
            &r.span,
        ));
    }
    Ok(p)
}

/// What an imported Sitefile's context is for `why`: a host directory, or a Git checkout's tree.
#[derive(Debug, Clone)]
pub enum UnitContext {
    Host(PathBuf),
    Tree(Tree),
}

/// A Sitefile imported AS sitefile: loaded, its imports resolved; its stages are built when first needed.
pub struct ImportedSitefile {
    /// The import's name as the build's own Sitefile sees it: `theme`, or `theme+inner` for one imported by `theme`.
    pub name: String,
    pub label: String,
    /// Its whole plan (every stage).
    pub plan: Plan,
    /// The sha256 of its Sitefile (a local directory) or the digest of the checked-out tree (Git).
    pub digest: String,
    pub context: UnitContext,
    /// Its imports by name, in IMPORT order (an `AS sitefile` one as its Sitefile).
    pub resources: Vec<(String, Resource)>,
    /// The Sitefiles it imports, in IMPORT order.
    pub imported: Vec<(String, ImportedSitefile)>,
    /// With `record_steps`: like [`BuildResult::steps`], for the stages built (a prefix of `plan.stages`).
    pub steps: Option<Vec<Vec<Tree>>>,
    /// What each of its FINGERPRINTs renamed (old path, new path), by its line.
    pub renames: Vec<(u32, Vec<(String, String)>)>,
    /// Each built named stage's FINGERPRINT renames, composed (inherited by `FROM <stage>`, also from the importer).
    stage_renames: HashMap<String, BTreeMap<String, String>>,
    local: LocalContext,
    resource_map: HashMap<String, Resource>,
    resource_keys: HashMap<String, String>,
    env_key: String,
    stage_trees: HashMap<String, Tree>,
    stage_keys: HashMap<String, String>,
    provenance: crate::resources::types::Provenance,
    sitefile_bytes: Vec<u8>,
    /// How many of its stages are built.
    built: usize,
}

impl ImportedSitefile {
    /// Whether a path of its context is a directory (`why` lists the file a directory COPY placed).
    pub fn is_directory(&self, rel: &str) -> bool {
        match &self.context {
            UnitContext::Host(dir) => {
                let mut p = dir.clone();
                for seg in rel.split('/').filter(|s| !s.is_empty()) {
                    p.push(seg);
                }
                p.is_dir()
            }
            UnitContext::Tree(t) => rel.is_empty() || t.is_directory(&format!("/{rel}")),
        }
    }

    /// The resource the import stands for (provenance): its Sitefile, with the unit's digest.
    fn resource(&self) -> Resource {
        Resource::File {
            bytes: self.sitefile_bytes.clone(),
            mode: 0o644,
            basename: "Sitefile".into(),
            provenance: crate::resources::types::Provenance {
                digest: self.digest.clone(),
                ..self.provenance.clone()
            },
        }
    }
}

/// The resources of the Sitefiles a build imports, by qualified name (`theme+data`), depth first in IMPORT order.
fn nested_resources(units: &[(String, ImportedSitefile)], out: &mut Vec<(String, Resource)>) {
    for (_, unit) in units {
        for (name, res) in &unit.resources {
            out.push((format!("{}+{name}", unit.name), res.clone()));
        }
        nested_resources(&unit.imported, out);
    }
}

/// What every Sitefile import of one build shares.
struct UnitsEnv<'a> {
    remote: RemoteEnv<'a>,
    memo: Option<&'a Mutex<BuildMemo>>,
    concurrency: usize,
    allow_hardlinks: bool,
    keep_os_metadata: bool,
    strict_pins: bool,
    /// Real paths the build writes or skips (export, vendor, --profile): left out of an imported context inside the build context.
    skip: Vec<PathBuf>,
    /// Temporary checkouts of Git imports, removed when the build ends.
    temp_dirs: &'a Mutex<Vec<PathBuf>>,
    record_steps: bool,
    /// Every remote input's pin under --strict-pins, by qualified import name.
    pins: Mutex<Vec<(String, &'static str)>>,
    plugin_policy: PluginPolicy,
    tool_policy: ToolPolicy,
    /// Resolve a `FROM image` stage base: (reference, AT, local, lead, span, the imported Sitefile's qualified name).
    from_image:
        &'a dyn Fn(&str, Option<&str>, bool, &str, &SourceSpan, Option<&str>) -> Result<FromImage>,
    cancel: Option<&'a CancelToken>,
    /// `BuildOptions::store_home` (the wasm module cache of imported Sitefiles' plugins).
    store_home: Option<PathBuf>,
}

/// Who imports a Sitefile: the build's own Sitefile or another imported one.
struct Importer<'a> {
    name: Option<&'a str>,
    label: &'a str,
    local: &'a LocalContext,
    scope: &'a LabelScope,
    /// (identity, label) of the Sitefiles importing this one, outermost first (the importer last).
    chain: &'a [(String, String)],
}

/// A new temporary directory (a Git import's checkout).
fn import_temp_dir() -> Result<PathBuf> {
    let base = std::env::temp_dir();
    for _ in 0..16 {
        let p = base.join(format!(
            "sitefile-import-{}",
            crate::export::exporter::random_hex(6)
        ));
        match std::fs::create_dir(&p) {
            Ok(()) => return Ok(p),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(SitefileError::plain(
                    E_EXPORT,
                    format!("cannot write a Git checkout to a temporary directory: {e}"),
                ))
            }
        }
    }
    Err(SitefileError::plain(
        E_EXPORT,
        "cannot write a Git checkout to a temporary directory: no unique name",
    ))
}

/// Write a tree to a new temporary directory (a Git import's checkout).
fn materialize_tree(tree: &Tree, env: &UnitsEnv<'_>) -> Result<PathBuf> {
    let dir = import_temp_dir()?;
    if let Ok(mut d) = env.temp_dirs.lock() {
        d.push(dir.clone());
    }
    let write = || -> std::io::Result<()> {
        for (p, n) in tree.entries() {
            let mut dest = dir.clone();
            for seg in p[1..].split('/') {
                dest.push(seg);
            }
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&dest, &n.bytes)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(n.mode))?;
            }
        }
        Ok(())
    };
    write().map_err(|e| {
        SitefileError::plain(
            E_EXPORT,
            format!("cannot write a Git checkout to a temporary directory: {e}"),
        )
    })?;
    Ok(dir)
}

fn not_a_sitefile_dir(spec: &ImportSpec, display: &str) -> SitefileError {
    SitefileError::at(E_TYPE, format!("IMPORT {}: {display} is a file; AS sitefile needs the directory that holds the Sitefile", spec.name), &spec.span)
}

fn not_a_sitefile_url(spec: &ImportSpec) -> SitefileError {
    SitefileError::at(
        E_PROTOCOL,
        format!("IMPORT {}: AS sitefile reads a context directory or a git+https:// repository, not a URL", spec.name),
        &spec.span,
    )
}

/// The cycle and depth checks of a Sitefile import (`id` is the imported Sitefile's identity).
fn check_import_chain(
    spec: &ImportSpec,
    chain: &[(String, String)],
    id: &str,
    label: &str,
) -> Result<()> {
    if let Some(seen) = chain.iter().position(|(c, _)| c == id) {
        let cycle: Vec<&str> = chain[seen..]
            .iter()
            .map(|(_, l)| l.as_str())
            .chain([label])
            .collect();
        return Err(SitefileError::at(
            E_VALIDATION,
            format!(
                "IMPORT {}: Sitefile import cycle: {}",
                spec.name,
                cycle.join(" -> ")
            ),
            &spec.span,
        ));
    }
    if chain.len() > MAX_SITEFILE_IMPORT_DEPTH {
        return Err(SitefileError::at(
            E_VALIDATION,
            format!("IMPORT {}: Sitefile imports nest more than {MAX_SITEFILE_IMPORT_DEPTH} levels deep", spec.name),
            &spec.span,
        ));
    }
    Ok(())
}

/// Load an `IMPORT name FROM <dir|git+https://…> AS sitefile`: find its context (a directory of the
/// importer's context, or a Git tree written to a temporary directory), read and plan its Sitefile with the
/// IMPORT's ARGs, check the stages `refs` name, and resolve its imports exactly as a build would.
fn load_sitefile_import(
    spec: &ImportSpec,
    importer: &Importer<'_>,
    refs: &[ImportedStageRef],
    env: &UnitsEnv<'_>,
) -> Result<ImportedSitefile> {
    check_cancel(env.cancel)?;
    let qualified = match importer.name {
        Some(n) => format!("{n}+{}", spec.name),
        None => spec.name.clone(),
    };
    let origin = format!("{}:{} IMPORT {}", importer.label, spec.span.line, spec.name);
    let span = Some(&spec.span);
    let (host_dir, scope, id, display, provenance, git_tree) = match &spec.location {
        ImportLocation::Git(loc) => {
            let remote = RemoteEnv {
                label: importer.label,
                ..env.remote
            };
            let (commit, selection) = fetch_git_import(spec, loc, &remote)?;
            let display = format!("'{}'", loc.url);
            let GitSelection::Tree(files) = selection else {
                return Err(not_a_sitefile_dir(spec, &display));
            };
            let tree = Tree::from_files(
                files
                    .into_iter()
                    .map(|(p, b, m)| (p, make_file(b, origin.clone(), m))),
            )?;
            let scope = LabelScope {
                prefix: format!("git+{}#{commit}:", loc.repo),
                dir: loc.path.clone(),
            };
            let id = format!("{}{}", scope.prefix, loc.path);
            let provenance = crate::resources::types::Provenance {
                uri: spec.uri.clone(),
                resolved_url: Some(pinned_git_url(loc, &commit)),
                digest: String::new(),
                git_commit: Some(commit.clone()),
            };
            let dir = materialize_tree(&tree, env)?;
            (dir, scope, id, display, provenance, Some(tree))
        }
        ImportLocation::Local { path: rel } => {
            let display = if rel.is_empty() {
                "'./'".to_string()
            } else {
                format!("'./{rel}'")
            };
            if importer.local.stat(rel, span)? != EntryKind::Directory {
                return Err(not_a_sitefile_dir(spec, &display));
            }
            let dir = importer.local.real_path(rel, span)?;
            let scope = LabelScope {
                prefix: importer.scope.prefix.clone(),
                dir: join_path(&importer.scope.dir, rel),
            };
            let id = dir.to_string_lossy().into_owned();
            let provenance = crate::resources::types::Provenance {
                uri: spec.uri.clone(),
                resolved_url: None,
                digest: String::new(),
                git_commit: None,
            };
            (dir, scope, id, display, provenance, None)
        }
        ImportLocation::Remote { .. } => return Err(not_a_sitefile_url(spec)),
    };
    let label = sitefile_label(&scope);
    check_import_chain(spec, importer.chain, &id, &label)?;

    // Its context: the directory, sandboxed (its own .sitefileignore applies; what the build writes is skipped).
    let mut ignore = vec![".git".to_string()];
    for s in &env.skip {
        let rel = relative_path(&host_dir, s);
        if !rel.is_empty() && !rel.starts_with("..") && !rel.starts_with('/') {
            ignore.push(rel);
        }
    }
    let local = LocalContext::open(
        &host_dir,
        &LocalContextOptions {
            ignore: Some(ignore),
            sitefileignore: true,
            allow_hardlinks: env.allow_hardlinks,
        },
    )?;
    let sitefile_bytes = match local.read_file("Sitefile", span) {
        Ok(Resource::File { bytes, .. }) => bytes,
        Ok(_) => unreachable!("read_file returns a file"),
        Err(e) if e.code == E_NOT_FOUND => {
            return Err(SitefileError::at(
                E_NOT_FOUND,
                format!("IMPORT {}: {display} has no Sitefile", spec.name),
                &spec.span,
            ));
        }
        Err(e) => return Err(e),
    };
    let p = plan_imported_sitefile(
        spec,
        &String::from_utf8_lossy(&sitefile_bytes),
        &label,
        refs,
    )?;
    for w in &p.warnings {
        (env.remote.warn)(w);
    }
    if env.strict_pins {
        let pins = {
            let guard = env.remote.lock.lock().unwrap_or_else(|e| e.into_inner());
            check_pins(&p, guard.as_ref(), None)?
        };
        if let Ok(mut all) = env.pins.lock() {
            for (n, how) in pins.imports {
                all.push((format!("{qualified}+{n}"), how));
            }
        }
    }
    // Its FROM image lockfile entries are kept even when its stages are not built.
    for st in &p.stages {
        if let StageBase::Image {
            reference,
            local: false,
            lead,
            span,
            ..
        } = &st.base
        {
            let key = image_lock_key(reference, lead, span)?;
            if let Some(l) = env
                .remote
                .lock
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_mut()
            {
                l.mark_image_used(&key);
            }
        }
    }

    // Its imports, as a build of it would resolve them (every one, as for a --target build).
    let ienv = ImportEnv {
        local: &local,
        archive_limits: &env.remote.archive_limits,
        keep_os_metadata: env.keep_os_metadata,
        remote: RemoteEnv {
            label: &label,
            ..env.remote
        },
        memo: env.memo,
        cancel: env.cancel,
    };
    let plain: Vec<&ImportSpec> = p
        .imports
        .iter()
        .filter(|s| s.as_kind != Some(ImportKind::Sitefile))
        .collect();
    let resolved = map_limit(&plain, env.concurrency.max(1), |s| resolve_import(s, &ienv))?;
    let mut by_name: HashMap<String, (Resource, String)> = HashMap::new();
    for (s, (res, key)) in plain.iter().zip(resolved) {
        by_name.insert(s.name.clone(), (without_drafts(s, res)?, key));
    }
    // SCHEMA files are read from the imported Sitefile's own context.
    for s in &plain {
        if let (Some(_), Some((res, _))) = (&s.schema, by_name.get(&s.name)) {
            check_import_schema(s, res, &local)?;
        }
    }
    let mut resources: Vec<(String, Resource)> = Vec::new();
    let mut resource_keys: HashMap<String, String> = HashMap::new();
    let mut imported: Vec<(String, ImportedSitefile)> = Vec::new();
    let nested_refs = imported_stage_refs(&p.stages);
    let mut chain = importer.chain.to_vec();
    chain.push((id, label.clone()));
    let me = Importer {
        name: Some(&qualified),
        label: &label,
        local: &local,
        scope: &scope,
        chain: &chain,
    };
    for s in &p.imports {
        if let Some((res, key)) = by_name.remove(&s.name) {
            resources.push((s.name.clone(), sorted(s, res)?));
            resource_keys.insert(s.name.clone(), key);
            continue;
        }
        let unit = load_sitefile_import(s, &me, &nested_refs, env)?;
        resources.push((s.name.clone(), unit.resource()));
        let key = if env.memo.is_some() {
            memo_key(&["sitefile", &label, &format!("{s:?}"), &unit.digest])
        } else {
            String::new()
        };
        resource_keys.insert(s.name.clone(), key);
        imported.push((s.name.clone(), unit));
    }
    let env_key = match env.memo {
        Some(_) => {
            let mut vars: Vec<_> = p.vars.iter().collect();
            vars.sort_by(|a, b| a.0.cmp(b.0));
            // html LETs are part of it: they are not values, but a template that uses one renders their text.
            let fragments: Vec<(&str, &str)> = p
                .fragments
                .iter()
                .map(|f| (f.name.as_str(), f.text.as_str()))
                .collect();
            memo_key(&[
                "env",
                &label,
                &format!("{vars:?}"),
                &format!("{fragments:?}"),
            ])
        }
        None => String::new(),
    };
    let digest = match &git_tree {
        Some(t) => t.digest(),
        None => sha256(&sitefile_bytes),
    };
    let context = match git_tree {
        Some(t) => UnitContext::Tree(t),
        None => UnitContext::Host(host_dir),
    };
    Ok(ImportedSitefile {
        name: qualified,
        label,
        plan: p,
        digest,
        context,
        resource_map: resources.iter().cloned().collect(),
        resources,
        imported,
        steps: env.record_steps.then(Vec::new),
        renames: Vec::new(),
        stage_renames: HashMap::new(),
        local,
        resource_keys,
        env_key,
        stage_trees: HashMap::new(),
        stage_keys: HashMap::new(),
        provenance,
        sitefile_bytes,
        built: 0,
    })
}

/// Make the imported stages `refs` names available in `stages`/`keys` as `<import>+<stage>`: each imported
/// Sitefile builds its stages up to the last one referenced (like a --target build).
fn provide_imported_stages(
    units: &mut [(String, ImportedSitefile)],
    refs: &[ImportedStageRef],
    stages: &mut HashMap<String, Tree>,
    keys: &mut HashMap<String, String>,
    renames: &mut HashMap<String, BTreeMap<String, String>>,
    env: &UnitsEnv<'_>,
) -> Result<()> {
    for (name, unit) in units.iter_mut() {
        let upto = refs
            .iter()
            .filter(|r| r.import == *name)
            .filter_map(|r| {
                unit.plan
                    .stages
                    .iter()
                    .position(|s| s.name.as_deref() == Some(r.stage.as_str()))
            })
            .max();
        let Some(upto) = upto else { continue };
        build_unit_stages(unit, upto, env)?;
        for st in &unit.plan.stages[..unit.built] {
            let Some(n) = &st.name else { continue };
            if let Some(t) = unit.stage_trees.get(n) {
                stages.insert(format!("{name}+{n}"), t.clone());
            }
            keys.insert(
                format!("{name}+{n}"),
                unit.stage_keys.get(n).cloned().unwrap_or_default(),
            );
            renames.insert(
                format!("{name}+{n}"),
                unit.stage_renames.get(n).cloned().unwrap_or_default(),
            );
        }
    }
    Ok(())
}

/// Build an imported Sitefile's stages up to index `upto` (those already built are kept).
fn build_unit_stages(unit: &mut ImportedSitefile, upto: usize, env: &UnitsEnv<'_>) -> Result<()> {
    if upto < unit.built {
        return Ok(());
    }
    let mut stages = std::mem::take(&mut unit.stage_trees);
    let mut keys = std::mem::take(&mut unit.stage_keys);
    let refs = imported_stage_refs(&unit.plan.stages[..=upto]);
    let mut stage_renames = std::mem::take(&mut unit.stage_renames);
    let provided = provide_imported_stages(
        &mut unit.imported,
        &refs,
        &mut stages,
        &mut keys,
        &mut stage_renames,
        env,
    );
    if let Err(e) = provided {
        unit.stage_trees = stages;
        unit.stage_keys = keys;
        unit.stage_renames = stage_renames;
        return Err(e);
    }
    let warn = env.remote.warn.clone();
    let unit_name = unit.name.clone();
    let partials: Partials = Mutex::new(HashMap::new());
    let from_image = |r: &str, a: Option<&str>, l: bool, le: &str, s: &SourceSpan| {
        (env.from_image)(r, a, l, le, s, Some(&unit_name))
    };
    let mut exec = Exec {
        local: &unit.local,
        resources: &unit.resource_map,
        stages,
        vars: &unit.plan.vars,
        fragments: &unit.plan.fragments,
        partials: &partials,
        label: &unit.label,
        warn: warn.clone(),
        plugins: {
            let mut h = PluginHost::new(env.plugin_policy, move |m: &str| warn(m));
            h.set_store_home(env.store_home.clone());
            h
        },
        tools: HashMap::new(),
        tool_policy: env.tool_policy,
        cache: None,
        plugin_digests: HashMap::new(),
        stats: TransformStats::default(),
        profiler: None,
        memo: env.memo,
        resource_keys: unit.resource_keys.clone(),
        env_key: unit.env_key.clone(),
        stage_keys: keys,
        input_key: String::new(),
        renames: BTreeMap::new(),
        recorded: None,
        unrecorded: env.remote.warn.clone(),
        from_image: &from_image,
        last_renames: None,
        fetch_policy: env.remote.policy.clone(),
        cancel: env.cancel,
    };
    let mut steps: Vec<Vec<Tree>> = Vec::new();
    let mut built = unit.built;
    let mut renames: Vec<(u32, Vec<(String, String)>)> = Vec::new();
    let run = (|| -> Result<()> {
        for stage in &unit.plan.stages[unit.built..=upto] {
            let (start, mut key) = exec.start_stage(stage)?;
            let mut tree = start;
            let mut current = match &stage.base {
                StageBase::Stage { name } => stage_renames.get(name).cloned().unwrap_or_default(),
                _ => BTreeMap::new(),
            };
            let mut record = unit.steps.as_ref().map(|_| vec![tree.clone()]);
            for op in &stage.operations {
                exec.renames.clone_from(&current);
                let (out, k, _) = exec.execute_op(op, tree, &key)?;
                tree = out;
                key = k;
                if let Some(r) = exec.last_renames.take() {
                    current = compose_renames(&current, &r);
                    renames.push((op.span().line, r));
                }
                if let Some(r) = record.as_mut() {
                    r.push(tree.clone());
                }
            }
            if let Some(r) = record {
                steps.push(r);
            }
            if let Some(name) = &stage.name {
                stage_renames.insert(name.clone(), current);
                exec.stages.insert(name.clone(), tree);
                exec.stage_keys.insert(name.clone(), key);
            }
            built += 1;
        }
        Ok(())
    })();
    exec.plugins.close();
    let Exec {
        stages, stage_keys, ..
    } = exec;
    unit.stage_trees = stages;
    unit.stage_keys = stage_keys;
    unit.stage_renames = stage_renames;
    unit.renames.extend(renames);
    unit.built = built;
    if let Some(all) = unit.steps.as_mut() {
        all.extend(steps);
    }
    run
}

/// Where an imported Sitefile is read from by `vendor` and `outdated`, which never build it: a directory of
/// a local context, or a directory of a Git tree held in memory.
pub enum SitefileSource {
    Local {
        local: LocalContext,
        scope: LabelScope,
        label: String,
        chain: Vec<(String, String)>,
    },
    Tree {
        tree: Tree,
        dir: String,
        scope: LabelScope,
        label: String,
        chain: Vec<(String, String)>,
    },
}

impl SitefileSource {
    pub fn label(&self) -> &str {
        match self {
            SitefileSource::Local { label, .. } | SitefileSource::Tree { label, .. } => label,
        }
    }
    fn parts(&self) -> (&LabelScope, &[(String, String)]) {
        match self {
            SitefileSource::Local { scope, chain, .. }
            | SitefileSource::Tree { scope, chain, .. } => (scope, chain),
        }
    }
}

/// The source of the build's own Sitefile, for [`read_imported_sitefile`].
pub fn root_sitefile_source(
    options: &BuildOptions,
    label: &str,
    sitefile_path: &Path,
) -> Result<SitefileSource> {
    let context_real = std::fs::canonicalize(&options.context_dir).map_err(|_| {
        SitefileError::plain(
            E_NOT_FOUND,
            format!("build context '{}' does not exist", options.context_dir),
        )
    })?;
    let local = LocalContext::open(
        &context_real,
        &LocalContextOptions {
            ignore: Some(vec![".git".into()]),
            sitefileignore: true,
            allow_hardlinks: options.allow_hardlinks,
        },
    )?;
    let id = if options.sitefile.is_none() {
        context_real.to_string_lossy().into_owned()
    } else {
        realpath_loose(sitefile_path).to_string_lossy().into_owned()
    };
    Ok(SitefileSource::Local {
        local,
        scope: root_scope(options, label),
        label: label.to_string(),
        chain: vec![(id, label.to_string())],
    })
}

/// Read and plan the Sitefile an `IMPORT … AS sitefile` of `from` names, without building it (for `vendor`
/// and `outdated`): the same location, cycle, depth and ARG rules as a build. A Git one is fetched with
/// `fetch_git` (the commit a build would use). Returns its plan and the source its own imports read from.
pub fn read_imported_sitefile(
    spec: &ImportSpec,
    from: &SitefileSource,
    refs: &[ImportedStageRef],
    fetch_git: &mut dyn FnMut(&ImportSpec, &GitLocation) -> Result<(String, GitSelection)>,
) -> Result<(Plan, SitefileSource)> {
    let span = Some(&spec.span);
    let (from_scope, chain) = from.parts();
    enum Made {
        Local(LocalContext),
        Tree(Tree, String),
    }
    let (scope, id, display, bytes, made) = match &spec.location {
        ImportLocation::Git(loc) => {
            let (commit, selection) = fetch_git(spec, loc)?;
            let display = format!("'{}'", loc.url);
            let GitSelection::Tree(files) = selection else {
                return Err(not_a_sitefile_dir(spec, &display));
            };
            let tree = Tree::from_files(
                files
                    .into_iter()
                    .map(|(p, b, m)| (p, make_file(b, "git", m))),
            )?;
            let scope = LabelScope {
                prefix: format!("git+{}#{commit}:", loc.repo),
                dir: loc.path.clone(),
            };
            let id = format!("{}{}", scope.prefix, loc.path);
            let bytes = tree.get("/Sitefile").map(|n| n.bytes.to_vec());
            (scope, id, display, bytes, Made::Tree(tree, String::new()))
        }
        ImportLocation::Local { path: rel } => {
            let display = if rel.is_empty() {
                "'./'".to_string()
            } else {
                format!("'./{rel}'")
            };
            let scope = LabelScope {
                prefix: from_scope.prefix.clone(),
                dir: join_path(&from_scope.dir, rel),
            };
            match from {
                SitefileSource::Local { local: parent, .. } => {
                    if parent.stat(rel, span)? != EntryKind::Directory {
                        return Err(not_a_sitefile_dir(spec, &display));
                    }
                    let real = parent.real_path(rel, span)?;
                    let local = LocalContext::open(
                        &real,
                        &LocalContextOptions {
                            ignore: Some(vec![".git".into()]),
                            sitefileignore: true,
                            allow_hardlinks: false,
                        },
                    )?;
                    let bytes = match local.read_file("Sitefile", span) {
                        Ok(Resource::File { bytes, .. }) => Some(bytes),
                        Ok(_) => None,
                        Err(e) if e.code == E_NOT_FOUND => None,
                        Err(e) => return Err(e),
                    };
                    (
                        scope,
                        real.to_string_lossy().into_owned(),
                        display,
                        bytes,
                        Made::Local(local),
                    )
                }
                SitefileSource::Tree { tree, dir, .. } => {
                    let joined = join_path(dir, rel);
                    let sub = if joined == "." { String::new() } else { joined };
                    let at = if sub.is_empty() {
                        "/".to_string()
                    } else {
                        format!("/{sub}")
                    };
                    if tree.is_file(&at) {
                        return Err(not_a_sitefile_dir(spec, &display));
                    }
                    if !tree.is_directory(&at) {
                        return Err(SitefileError::at(
                            E_NOT_FOUND,
                            format!("{} does not exist in the build context", display),
                            &spec.span,
                        ));
                    }
                    let id = format!("{}{}", from_scope.prefix, scope.dir);
                    let file = if at == "/" {
                        "/Sitefile".to_string()
                    } else {
                        format!("{at}/Sitefile")
                    };
                    let bytes = tree.get(&file).map(|n| n.bytes.to_vec());
                    (scope, id, display, bytes, Made::Tree(tree.clone(), sub))
                }
            }
        }
        ImportLocation::Remote { .. } => return Err(not_a_sitefile_url(spec)),
    };
    let label = sitefile_label(&scope);
    check_import_chain(spec, chain, &id, &label)?;
    let Some(bytes) = bytes else {
        return Err(SitefileError::at(
            E_NOT_FOUND,
            format!("IMPORT {}: {display} has no Sitefile", spec.name),
            &spec.span,
        ));
    };
    let p = plan_imported_sitefile(spec, &String::from_utf8_lossy(&bytes), &label, refs)?;
    let mut chain = chain.to_vec();
    chain.push((id, label.clone()));
    let source = match made {
        Made::Local(local) => SitefileSource::Local {
            local,
            scope,
            label,
            chain,
        },
        Made::Tree(tree, dir) => SitefileSource::Tree {
            tree,
            dir,
            scope,
            label,
            chain,
        },
    };
    Ok((p, source))
}

// ---------------------------------------------------------------- plugins and tools

/// Check a plugin file (sandbox, own folder, integrity) and register it.
fn register_plugin(
    spec: &PluginSpec,
    local: &LocalContext,
    host: &mut PluginHost,
    permission: &PluginPermission,
) -> Result<()> {
    let span = Some(&spec.span);
    let (real, bytes) = local.real_file(&spec.path, span)?;
    if real.to_string_lossy().ends_with(".wasm") {
        // A wasm plugin has no imports, so it reads nothing: no own-folder rule needed, and INTEGRITY pins the module.
        if let Some(i) = &spec.integrity {
            verify_integrity(&bytes, i, &format!("./{}", spec.path), span)?;
        }
        check_wasm_plugin_in(host.store_home(), &spec.name, &bytes, span)?;
        host.register(&spec.name, real, Some(spec.span.clone()), Some(bytes));
        return Ok(());
    }
    // It runs as JavaScript (also a '.wasm' path that is a symlink to a JS file), so it needs the opt-in.
    permission.check(spec)?;
    // A JS plugin may read its whole folder, so that folder must not be the context itself.
    if real.parent() == Some(local.root()) {
        let base = real
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        return Err(SitefileError::at(
            E_VALIDATION,
            format!("PLUGIN {}: put the plugin in its own folder (e.g. ./plugins/{base}); a plugin can read every file next to it", spec.name),
            &spec.span,
        ));
    }
    // It can read and import everything in its folder, so INTEGRITY pins the whole folder (#158).
    if let Some(expected) = &spec.integrity {
        let dir = real.parent().unwrap_or(local.root());
        let rel = dir.strip_prefix(local.root()).unwrap_or(dir);
        let rel: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        let shown = format!("./{}/", rel.join("/"));
        let r = crate::plugins::cache::plugin_folder_integrity(dir).map_err(|e| {
            SitefileError::at(
                E_INTEGRITY,
                format!(
                    "PLUGIN {}: could not read the plugin's folder {shown}: {e}",
                    spec.name
                ),
                &spec.span,
            )
        })?;
        match r {
            Err(problem) => {
                return Err(SitefileError::at(
                    E_INTEGRITY,
                    format!(
                        "PLUGIN {}: INTEGRITY pins the plugin's folder {shown}, which {problem}",
                        spec.name
                    ),
                    &spec.span,
                ))
            }
            Ok(digest) if digest != expected.to_ascii_lowercase() => {
                return Err(SitefileError::at(
                    E_INTEGRITY,
                    format!("integrity mismatch for {shown} (PLUGIN {}'s folder: INTEGRITY covers every file in it): expected {expected}, got {digest}", spec.name),
                    &spec.span,
                ))
            }
            Ok(_) => {}
        }
    }
    host.register(&spec.name, real, Some(spec.span.clone()), None);
    Ok(())
}

fn is_identifier(s: &str) -> bool {
    let mut c = s.chars();
    c.next()
        .is_some_and(|f| f.is_ascii_alphabetic() || f == '_')
        && c.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
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
        )
        .with_needs(Needs::plugin(&spec.name)))
    }
}

/// Check every JavaScript PLUGIN against `--allow-plugin` / `--allow-plugins` before
/// anything is read or started. A wasm plugin (a module that imports nothing) runs
/// without permission. Returns the check, which register_plugin repeats on the real path.
fn check_plugins(p: &Plan, options: &BuildOptions, warn: &Warn) -> Result<PluginPermission> {
    let mut names: Vec<String> = Vec::new();
    for name in &options.allow_plugin {
        if !is_identifier(name) {
            return Err(SitefileError::plain(
                E_USAGE,
                format!("--allow-plugin expects a PLUGIN name (an identifier), got '{name}'"),
            ));
        }
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    let permission = PluginPermission {
        all: options.allow_all_plugins,
        names,
    };
    for spec in &p.plugins {
        if !spec.path.ends_with(".wasm") {
            permission.check(spec)?;
        }
    }
    for name in &permission.names {
        if !p.plugins.iter().any(|s| &s.name == name) {
            warn(&format!(
                "--allow-plugin {name} does not match any PLUGIN and was ignored"
            ));
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
            )
            .with_needs(Needs::tool(&t.name)));
        };
        let tool = resolve_tool(
            &t.name,
            explicit.as_deref(),
            search_path.as_deref(),
            Some(&t.span),
        )?;
        if let Some(n) = &options.on_notice {
            n(&format!("tool {} -> {}", t.name, tool.path.display()));
        }
        out.push(tool);
    }
    for (name, _) in &allowed {
        if !p.tools.iter().any(|t| &t.name == name) {
            warn(&format!(
                "--allow-exec {name} does not match any TOOL and was ignored"
            ));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- operations

/// `IMPORT … AS html` templates, parsed on first use.
type Partials = Mutex<HashMap<String, Arc<Vec<TemplateNode>>>>;

/// Variables visible to interpolation and templates.
#[derive(Clone, Copy)]
struct Scope<'a> {
    vars: &'a Env,
    resources: &'a HashMap<String, Resource>,
    /// html LETs (`LET name: html`), parsed by the planner.
    fragments: &'a [FragmentSpec],
    partials: &'a Partials,
    /// The current element of a FOR loop.
    each: Option<(&'a str, &'a Value)>,
}

impl<'a> Scope<'a> {
    fn with_item(self, name: &'a str, value: &'a Value) -> Scope<'a> {
        Scope {
            each: Some((name, value)),
            ..self
        }
    }

    /// Whether `name` is an html fragment: an html LET or an `IMPORT … AS html`.
    fn is_fragment(&self, name: &str) -> bool {
        if self.each.is_some_and(|(n, _)| n == name) {
            return false;
        }
        self.fragments.iter().any(|f| f.name == name)
            || matches!(
                self.resources.get(name),
                Some(Resource::Value {
                    format: ValueFormat::Html,
                    ..
                })
            )
    }

    /// The parsed template of fragment `name` (an imported partial is parsed on first use).
    fn fragment(&self, name: &str) -> Result<Arc<Vec<TemplateNode>>> {
        if let Some(f) = self.fragments.iter().find(|f| f.name == name) {
            return Ok(f.nodes.clone());
        }
        let Some(Resource::Value {
            format: ValueFormat::Html,
            value,
            provenance,
        }) = self.resources.get(name)
        else {
            return Ok(Arc::new(Vec::new()));
        };
        if let Some(n) = self.partials.lock().ok().and_then(|m| m.get(name).cloned()) {
            return Ok(n);
        }
        let text = crate::language::validate::fragment_text(value.as_str().unwrap_or_default());
        let nodes = Arc::new(parse_render_template(text, &provenance.uri, 1)?);
        if let Ok(mut m) = self.partials.lock() {
            m.insert(name.to_string(), nodes.clone());
        }
        Ok(nodes)
    }

    /// Render a parsed template against this scope (values, blocks, html fragments and, with
    /// `files`, `${file …}` on the stage's tree and RENDER --inline-svg).
    fn render(
        &self,
        nodes: &[TemplateNode],
        escape: crate::language::ast::EscapeMode,
        files: (&dyn crate::media::TemplateTree, bool),
    ) -> Result<String> {
        let ctx = TemplateContext {
            is_fragment: &|n: &str| self.is_fragment(n),
            fragment: &|n: &str| self.fragment(n),
            tree: Some(files.0),
            inline_svg: files.1,
        };
        render_template_with(
            nodes,
            &|n: &str, p: &[String], s: &SourceSpan| self.lookup(n, p, s),
            escape,
            Some(&|n: &str, p: &[String], s: &SourceSpan, l: bool| self.value_of(n, p, s, l)),
            Some(&ctx),
        )
    }

    /// The names a template reads, with those of the html fragments it uses (and theirs), for memo
    /// keys. A fragment that does not parse adds nothing: rendering fails on it anyway.
    /// Also: whether the template, or a fragment it uses, has a `${file …}` (it then reads the tree).
    fn render_deps(&self, nodes: &[TemplateNode]) -> (BTreeSet<String>, bool) {
        let mut out = template_names(nodes);
        let mut files = uses_files(nodes);
        let mut seen: BTreeSet<String> = BTreeSet::new();
        loop {
            let before = out.len();
            for name in out.clone() {
                if seen.contains(&name) || !self.is_fragment(&name) {
                    continue;
                }
                seen.insert(name.clone());
                if let Ok(nodes) = self.fragment(&name) {
                    out.extend(template_names(&nodes));
                    files |= uses_files(&nodes);
                }
            }
            if out.len() == before {
                return (out, files);
            }
        }
    }

    fn lookup(&self, name: &str, p: &[String], span: &SourceSpan) -> Result<Scalar> {
        if let Some((n, v)) = self.each {
            if n == name {
                return resolve_json_path(v, p, name, span);
            }
        }
        if let Some(v) = self.vars.get(name) {
            if !p.is_empty() {
                return Err(SitefileError::at(
                    E_TYPE,
                    format!(
                        "'{name}' is an ARG/LET value; dotted access only works on JSON imports"
                    ),
                    span,
                ));
            }
            return Ok(v.clone());
        }
        let Some(res) = self.resources.get(name) else {
            return Err(SitefileError::at(
                E_UNDEFINED,
                format!("'{name}' is not an ARG, LET or IMPORT"),
                span,
            ));
        };
        let Resource::Value { format, value, .. } = res else {
            return Err(SitefileError::at(
                E_TYPE,
                format!("import '{name}' is a {}; templates can only read value imports (text, json, yaml, csv, markdown, wbn)", res.kind()),
                span,
            ));
        };
        if *format == ValueFormat::Html {
            return Err(fragment_not_value(name, span));
        }
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
    fn value_of(
        &self,
        name: &str,
        p: &[String],
        span: &SourceSpan,
        loose: bool,
    ) -> Result<Option<Cow<'a, Value>>> {
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
            return Err(SitefileError::at(
                E_UNDEFINED,
                format!("'{name}' is not an ARG, LET or IMPORT"),
                span,
            ));
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
                TemplatePart::Var { name, path, span } => {
                    out.push_str(&self.lookup(name, path, span)?.to_js_string())
                }
            }
        }
        Ok(out)
    }
}

/// The path `--parents` keeps: context-relative for a local source, inside the import or stage otherwise (`""` for a root).
fn source_path(r: &SourceRef) -> String {
    match r {
        SourceRef::Local { path, .. } => path.clone(),
        SourceRef::Inline { .. } => String::new(),
        SourceRef::Stage { path, .. } | SourceRef::Import { path, .. } => {
            path.trim_start_matches('/').to_string()
        }
    }
}

/// A directory source without the files `exclude` (COPY --exclude) matches, relative to its root.
fn excluding(tree: Tree, exclude: Option<&IgnoreRules>) -> Result<Tree> {
    let Some(rules) = exclude else {
        return Ok(tree);
    };
    Tree::from_files(
        tree.entries()
            .filter(|(p, _)| !rules.excludes(&p[1..]))
            .map(|(p, n)| (p.clone(), n.clone())),
    )
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
    fragments: &'a [FragmentSpec],
    partials: &'a Partials,
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
    /// With a memo: the key of the tree the running operation reads (a RENDER with `${file …}` depends on it).
    input_key: String,
    /// What the stage's FINGERPRINTs so far renamed (old path -> new path): `${file "/style.css"}` after one finds the hashed file.
    renames: BTreeMap<String, String>,
    /// While an operation runs with a memo: the warnings it printed (a memoized step replays them).
    recorded: Option<Arc<Mutex<Vec<String>>>>,
    /// The warning callback that does not record (a tool's stderr).
    unrecorded: Warn,
    /// Resolve a `FROM image` stage base: (reference, AT, local, lead, span).
    from_image: &'a dyn Fn(&str, Option<&str>, bool, &str, &SourceSpan) -> Result<FromImage>,
    /// What the FINGERPRINT that just ran renamed (old path, new path).
    last_renames: Option<Vec<(String, String)>>,
    /// The network policy (CHECKLINKS --external).
    fetch_policy: FetchPolicy,
    /// `BuildOptions::cancel`.
    cancel: Option<&'a CancelToken>,
}

/// What a stage starts from, for the trace.
fn stage_from(stage: &Stage) -> String {
    match &stage.base {
        StageBase::Scratch => "scratch".into(),
        StageBase::Stage { name } => name.clone(),
        StageBase::Local { path } if path.is_empty() => "./".into(),
        StageBase::Local { path } => format!("./{path}"),
        StageBase::Image {
            reference,
            at: Some(at),
            ..
        } => format!("image {reference} AT {at}"),
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
        Scope {
            vars: self.vars,
            resources: self.resources,
            fragments: self.fragments,
            partials: self.partials,
            each: None,
        }
    }

    /// The tree a stage starts from, and its key.
    fn start_stage(&self, stage: &Stage) -> Result<(Tree, String)> {
        check_cancel(self.cancel)?;
        match &stage.base {
            StageBase::Scratch => Ok((
                Tree::empty(),
                if self.memo.is_some() {
                    memo_key(&["scratch", &self.env_key])
                } else {
                    String::new()
                },
            )),
            StageBase::Stage { name } => {
                let tree = self
                    .stages
                    .get(name)
                    .cloned()
                    .ok_or_else(|| unknown_stage(name, &stage.span))?;
                Ok((tree, self.stage_keys.get(name).cloned().unwrap_or_default()))
            }
            StageBase::Local { path } => {
                let display = if path.is_empty() {
                    "./".to_string()
                } else {
                    format!("./{path}")
                };
                if self.local.stat(path, Some(&stage.span))? != EntryKind::Directory {
                    return Err(SitefileError::at(
                        E_TYPE,
                        format!("FROM '{display}' is a file; FROM needs a directory"),
                        &stage.span,
                    ));
                }
                match self.local.read_tree(
                    path,
                    &format!("{}:{} FROM", self.label, stage.span.line),
                    Some(&stage.span),
                )? {
                    Resource::Tree { tree, provenance } => {
                        let key = if self.memo.is_some() {
                            memo_key(&[
                                "from",
                                &self.env_key,
                                &stage.span.line.to_string(),
                                path,
                                &provenance.digest,
                            ])
                        } else {
                            String::new()
                        };
                        Ok((tree, key))
                    }
                    _ => unreachable!("read_tree returns a tree"),
                }
            }
            StageBase::Image {
                reference,
                local,
                at,
                lead,
                span,
            } => {
                let img = (self.from_image)(reference, at.as_deref(), *local, lead, span)?;
                let key = if self.memo.is_some() {
                    memo_key(&[
                        "image",
                        &self.env_key,
                        &stage.span.line.to_string(),
                        reference,
                        at.as_deref().unwrap_or(""),
                        &img.digest,
                    ])
                } else {
                    String::new()
                };
                Ok((img.tree, key))
            }
        }
    }

    /// `exclude` (COPY --exclude) drops the matching files of a directory source (a local one is walked without them).
    fn read_source(
        &self,
        r: &SourceRef,
        origin: &str,
        exclude: Option<&IgnoreRules>,
    ) -> Result<Source> {
        match r {
            SourceRef::Inline {
                text,
                delimiter,
                body_line,
                ..
            } => Ok(Source::File {
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
                    let Resource::Tree { tree, provenance } =
                        self.local
                            .read_tree_excluding(path, origin, Some(span), exclude)?
                    else {
                        unreachable!()
                    };
                    return Ok(Source::Tree {
                        tree,
                        label,
                        digest: provenance.digest,
                    });
                }
                let Resource::File {
                    bytes,
                    mode,
                    basename,
                    provenance,
                } = self.local.read_file(path, Some(span))?
                else {
                    unreachable!()
                };
                let digest = format!("{}\0{mode}\0{basename}", provenance.digest);
                Ok(Source::File {
                    bytes: Arc::from(bytes),
                    mode,
                    basename,
                    label,
                    inline: None,
                    digest,
                })
            }
            SourceRef::Stage { name, path, span } => {
                let tree = self
                    .stages
                    .get(name)
                    .ok_or_else(|| unknown_stage(name, span))?;
                let label = format!("--from={name} {path}");
                if let Some(node) = tree.get(path) {
                    return Ok(Source::File {
                        bytes: node.bytes.clone(),
                        mode: node.mode,
                        basename: basename_virtual(path).to_string(),
                        label,
                        inline: None,
                        digest: String::new(),
                    });
                }
                if tree.is_directory(path) {
                    return Ok(Source::Tree {
                        tree: excluding(tree.subtree(path), exclude)?,
                        label,
                        digest: String::new(),
                    });
                }
                Err(SitefileError::at(
                    E_NOT_FOUND,
                    format!("'{path}' does not exist in stage '{name}'"),
                    span,
                ))
            }
            SourceRef::Import { name, path, span } => {
                let label = if path == "/" {
                    format!("{name}:")
                } else {
                    format!("{name}:{path}")
                };
                let res = self.resources.get(name).ok_or_else(|| {
                    SitefileError::at(E_UNDEFINED, format!("unknown import '{name}'"), span)
                })?;
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
                            return Ok(Source::Tree { tree: excluding(tree.subtree(path), exclude)?, label, digest: String::new() });
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
            SourceRef::Stage { name, .. } => format!(
                "stage\0{}",
                self.stage_keys.get(name).map(String::as_str).unwrap_or("")
            ),
            SourceRef::Import { name, .. } => format!(
                "import\0{}",
                self.resource_keys
                    .get(name)
                    .map(String::as_str)
                    .unwrap_or("")
            ),
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
        let (Some(memo), Some(k)) = (self.memo, key) else {
            return None;
        };
        let (files, warnings) = lock_memo(memo).get::<(Files, Vec<String>)>(k)?;
        let seen = self
            .recorded
            .as_ref()
            .and_then(|r| r.lock().ok().map(|r| r.len()))
            .unwrap_or(0);
        for w in warnings.iter().skip(seen) {
            (self.warn)(w);
        }
        Some(Produced {
            files,
            key: Some(k.clone()),
            hit: true,
        })
    }

    /// Memoize a step's files under `key` (with the warnings it printed), unless `store` is false.
    fn memo_store(
        &mut self,
        key: Option<String>,
        files: Files,
        children: Vec<String>,
        store: bool,
    ) -> Produced {
        let (Some(memo), Some(k), true) = (self.memo, key, store) else {
            return Produced {
                files,
                key: None,
                hit: false,
            };
        };
        let warnings = self
            .recorded
            .as_ref()
            .and_then(|r| r.lock().ok().map(|r| r.clone()))
            .unwrap_or_default();
        lock_memo(memo).set(k.clone(), (files.clone(), warnings), children);
        Produced {
            files,
            key: Some(k),
            hit: false,
        }
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
    fn execute_op(
        &mut self,
        op: &Operation,
        tree: Tree,
        prev: &str,
    ) -> Result<(Tree, String, Option<&'static str>)> {
        check_cancel(self.cancel)?;
        let span = op.span();
        let Some(memo) = self.memo else {
            if let Operation::Sitemap { .. } = op {
                return Ok((self.sitemap(op, tree)?, String::new(), None));
            }
            if let Operation::Canonical { .. } = op {
                let out = self.canonical(op, tree, &*self.warn.clone())?;
                return Ok((out, String::new(), None));
            }
            if let Operation::Precompress { .. } = op {
                let out = self.precompress(op, tree, &*self.warn.clone())?;
                return Ok((out, String::new(), None));
            }
            if let Operation::Precache { .. } = op {
                let out = self.precache(op, tree, &*self.warn.clone())?;
                return Ok((out, String::new(), None));
            }
            if let Operation::CheckLinks { .. } = op {
                self.check_links(op, &tree, &*self.warn.clone())?;
                return Ok((tree, String::new(), None));
            }
            if let Operation::Fingerprint { .. } = op {
                let (out, renames) = self.fingerprint(op, &tree, &*self.warn.clone())?;
                self.last_renames = Some(renames);
                return Ok((out, String::new(), None));
            }
            let p = self.produce(op, &tree, "")?;
            return Ok((
                tree.with_files(p.files, policy_of(op_overwrite(op)), Some(span))?,
                String::new(),
                None,
            ));
        };
        let op_id = memo_key(&["op", &self.env_key, &format!("{op:?}")]);
        if let Operation::CheckLinks { .. } = op {
            // Reads the whole tree; the tree is unchanged. A hit skips the check (and its requests) and prints its warnings again.
            let key = memo_key(&["chain", prev, &op_id]);
            if let Some(warnings) = lock_memo(memo).get::<Vec<String>>(&key) {
                for w in &warnings {
                    (self.warn)(w);
                }
                return Ok((tree, key, Some("hit")));
            }
            let printed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let outer = self.warn.clone();
            let p2 = printed.clone();
            let record = move |m: &str| {
                if let Ok(mut v) = p2.lock() {
                    v.push(m.to_string());
                }
                outer(m)
            };
            self.check_links(op, &tree, &record)?;
            let warnings = printed.lock().map(|v| v.clone()).unwrap_or_default();
            lock_memo(memo).set(key.clone(), warnings, Vec::new());
            return Ok((tree, key, Some("miss")));
        }
        if let Operation::Fingerprint { .. } = op {
            // Reads the whole tree: only the chain key can say it is unchanged.
            let key = memo_key(&["chain", prev, &op_id]);
            type Hit = (Tree, Vec<(String, String)>, Vec<String>);
            if let Some((hit, renames, warnings)) = lock_memo(memo).get::<Hit>(&key) {
                for w in &warnings {
                    (self.warn)(w);
                }
                self.last_renames = Some(renames);
                return Ok((hit, key, Some("hit")));
            }
            let printed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let outer = self.warn.clone();
            let p2 = printed.clone();
            let record = move |m: &str| {
                if let Ok(mut v) = p2.lock() {
                    v.push(m.to_string());
                }
                outer(m)
            };
            let (out, renames) = self.fingerprint(op, &tree, &record)?;
            let warnings = printed.lock().map(|v| v.clone()).unwrap_or_default();
            lock_memo(memo).set(
                key.clone(),
                (out.clone(), renames.clone(), warnings),
                Vec::new(),
            );
            self.last_renames = Some(renames);
            return Ok((out, key, Some("miss")));
        }
        if let Operation::Canonical { .. } = op {
            // Reads the whole tree: only the chain key can say it is unchanged; a hit prints its warnings again.
            let key = memo_key(&["chain", prev, &op_id]);
            type Hit = (Tree, Vec<String>);
            if let Some((hit, warnings)) = lock_memo(memo).get::<Hit>(&key) {
                for w in &warnings {
                    (self.warn)(w);
                }
                return Ok((hit, key, Some("hit")));
            }
            let printed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let outer = self.warn.clone();
            let p2 = printed.clone();
            let record = move |m: &str| {
                if let Ok(mut v) = p2.lock() {
                    v.push(m.to_string());
                }
                outer(m)
            };
            let out = self.canonical(op, tree, &record)?;
            let warnings = printed.lock().map(|v| v.clone()).unwrap_or_default();
            lock_memo(memo).set(key.clone(), (out.clone(), warnings), Vec::new());
            return Ok((out, key, Some("miss")));
        }
        if let Operation::Precompress { .. } = op {
            // Reads the whole tree: only the chain key can say it is unchanged; a hit prints its warnings again.
            let key = memo_key(&["chain", prev, &op_id]);
            type Hit = (Tree, Vec<String>);
            if let Some((hit, warnings)) = lock_memo(memo).get::<Hit>(&key) {
                for w in &warnings {
                    (self.warn)(w);
                }
                return Ok((hit, key, Some("hit")));
            }
            let printed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let outer = self.warn.clone();
            let p2 = printed.clone();
            let record = move |m: &str| {
                if let Ok(mut v) = p2.lock() {
                    v.push(m.to_string());
                }
                outer(m)
            };
            let out = self.precompress(op, tree, &record)?;
            let warnings = printed.lock().map(|v| v.clone()).unwrap_or_default();
            lock_memo(memo).set(key.clone(), (out.clone(), warnings), Vec::new());
            return Ok((out, key, Some("miss")));
        }
        if let Operation::Precache { .. } = op {
            // The chain key covers the stage's earlier steps, so also what its FINGERPRINTs renamed.
            let key = memo_key(&["chain", prev, &op_id]);
            type Hit = (Tree, Vec<String>);
            if let Some((hit, warnings)) = lock_memo(memo).get::<Hit>(&key) {
                for w in &warnings {
                    (self.warn)(w);
                }
                return Ok((hit, key, Some("hit")));
            }
            let printed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let outer = self.warn.clone();
            let p2 = printed.clone();
            let record = move |m: &str| {
                if let Ok(mut v) = p2.lock() {
                    v.push(m.to_string());
                }
                outer(m)
            };
            let out = self.precache(op, tree, &record)?;
            let warnings = printed.lock().map(|v| v.clone()).unwrap_or_default();
            lock_memo(memo).set(key.clone(), (out.clone(), warnings), Vec::new());
            return Ok((out, key, Some("miss")));
        }
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
        self.input_key = prev.to_string();
        let p = self.produce_recorded(op, &tree, &op_id)?;
        // Not memoizable (a plugin that opted out of caching): chain on what it produced instead.
        let step_key = match &p.key {
            Some(k) => k.clone(),
            None => {
                let mut parts: Vec<String> = vec!["files".into()];
                for (f, n) in &p.files {
                    parts.extend([
                        f.clone(),
                        n.mode.to_string(),
                        n.digest.clone(),
                        n.origin.clone(),
                    ]);
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

    fn check_links(&self, op: &Operation, tree: &Tree, warn: &dyn Fn(&str)) -> Result<()> {
        let Operation::CheckLinks {
            warn: as_warnings,
            external,
            root,
            redirects,
            span,
        } = op
        else {
            unreachable!()
        };
        let op = CheckLinksOp {
            warn: *as_warnings,
            external: *external,
            root,
            redirects,
            span,
        };
        let broken = broken_links(tree, &op, &self.fetch_policy);
        report_broken_links(
            &broken,
            &op,
            &format!("{}:{} CHECKLINKS", self.label, span.line),
            warn,
        )
    }

    fn fingerprint(
        &self,
        op: &Operation,
        tree: &Tree,
        warn: &dyn Fn(&str),
    ) -> Result<(Tree, Vec<(String, String)>)> {
        let Operation::Fingerprint {
            patterns,
            manifest,
            integrity,
            root,
            overwrite,
            span,
        } = op
        else {
            unreachable!()
        };
        let origin = format!("{}:{} FINGERPRINT", self.label, span.line);
        let op = FingerprintOp {
            patterns,
            manifest: manifest.as_deref(),
            integrity: *integrity,
            root,
            overwrite: *overwrite,
            span,
        };
        fingerprint(tree, &op, &origin, warn)
    }

    fn canonical(&self, op: &Operation, tree: Tree, warn: &dyn Fn(&str)) -> Result<Tree> {
        let Operation::Canonical {
            base,
            pretty,
            root,
            span,
        } = op
        else {
            unreachable!()
        };
        let origin = format!("{}:{} CANONICAL", self.label, span.line);
        let op = CanonicalOp {
            base,
            pretty: *pretty,
            root,
            span,
        };
        canonical(tree, &op, &origin, warn)
    }

    fn precompress(&self, op: &Operation, tree: Tree, warn: &dyn Fn(&str)) -> Result<Tree> {
        let Operation::Precompress {
            patterns,
            min,
            overwrite,
            span,
            ..
        } = op
        else {
            unreachable!()
        };
        let origin = format!("{}:{} PRECOMPRESS", self.label, span.line);
        let op = PrecompressOp {
            patterns,
            min: *min,
            overwrite: *overwrite,
            span,
        };
        precompress(tree, &op, &origin, warn)
    }

    fn precache(&self, op: &Operation, tree: Tree, warn: &dyn Fn(&str)) -> Result<Tree> {
        let Operation::Precache {
            patterns,
            dest,
            into,
            root,
            overwrite,
            span,
        } = op
        else {
            unreachable!()
        };
        let origin = format!("{}:{} PRECACHE", self.label, span.line);
        let fingerprinted: HashSet<String> = self.renames.values().cloned().collect();
        let op = PrecacheOp {
            patterns,
            dest: dest.as_deref(),
            into: into.as_deref(),
            root,
            overwrite: *overwrite,
            span,
        };
        precache(tree, &op, &fingerprinted, &origin, warn)
    }

    fn sitemap(&self, op: &Operation, tree: Tree) -> Result<Tree> {
        let Operation::Sitemap {
            base,
            dest,
            overwrite,
            span,
        } = op
        else {
            unreachable!()
        };
        let origin = format!("{}:{} SITEMAP", self.label, span.line);
        let files: Vec<(String, Node)> = sitemap_files(&tree, base, dest, SITEMAP_LIMITS)
            .into_iter()
            .map(|(p, xml)| (p, make_file(xml.into_bytes(), origin.as_str(), 0o644)))
            .collect();
        tree.with_files(files, policy_of(*overwrite), Some(span))
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
            Operation::TextFile { dest, text, .. } => {
                // The planner wrote the text; it reads nothing.
                let key = memoized.then(|| memo_key(&["text", op_id]));
                if let Some(hit) = self.memo_lookup(&key) {
                    return Ok(hit);
                }
                let files: Files = vec![(
                    dest.clone(),
                    make_file(text.clone().into_bytes(), origin, 0o644),
                )];
                Ok(self.memo_store(key, files, Vec::new(), true))
            }
            Operation::Sitemap { .. } => unreachable!("SITEMAP reads the tree: execute_op runs it"),
            Operation::Fingerprint { .. } => {
                unreachable!("FINGERPRINT reads the tree: execute_op runs it")
            }
            Operation::Canonical { .. } => {
                unreachable!("CANONICAL reads the tree: execute_op runs it")
            }
            Operation::Precompress { .. } => {
                unreachable!("PRECOMPRESS reads the tree: execute_op runs it")
            }
            Operation::Precache { .. } => {
                unreachable!("PRECACHE reads the tree: execute_op runs it")
            }
            Operation::CheckLinks { .. } => {
                unreachable!("CHECKLINKS reads the tree: execute_op runs it")
            }
            Operation::Copy {
                source,
                dest,
                dest_is_dir,
                exclude,
                chmod,
                parents,
                span,
                ..
            } => {
                let rules = (!exclude.is_empty()).then(|| IgnoreRules::from_patterns(exclude));
                let src = self.read_source(source, &origin, rules.as_ref())?;
                // --parents: the source's own path (inside the context, import or stage) is kept under the destination.
                let kept = if *parents {
                    source_path(source)
                } else {
                    String::new()
                };
                if let (Some(_), Source::File { label, .. }) = (&rules, &src) {
                    return Err(SitefileError::at(
                        E_TYPE,
                        format!("--exclude filters a directory, but '{label}' is a file"),
                        source.span(),
                    ));
                }
                let into_dir = matches!(src, Source::File { .. })
                    && (*parents || *dest_is_dir || tree.is_directory(dest));
                let key = memoized.then(|| {
                    let shape = if matches!(src, Source::File { .. }) {
                        into_dir.to_string()
                    } else {
                        "tree".into()
                    };
                    memo_key(&["copy", op_id, &self.source_key(source, &src), &shape])
                });
                if let Some(hit) = self.memo_lookup(&key) {
                    return Ok(hit);
                }
                let files: Files = match src {
                    Source::File {
                        bytes,
                        mode,
                        basename,
                        label,
                        inline,
                        ..
                    } => {
                        if into_dir && inline.is_some() {
                            return Err(SitefileError::at(E_PATH, format!("'{dest}' is a directory; an inline {label} body needs a file destination"), span));
                        }
                        let target = if into_dir {
                            join_virtual(dest, if kept.is_empty() { &basename } else { &kept })
                        } else {
                            dest.clone()
                        };
                        vec![(target, make_file(bytes, origin, chmod.unwrap_or(mode)))]
                    }
                    Source::Tree {
                        tree: src, label, ..
                    } => {
                        if src.is_empty() {
                            (self.warn)(&if rules.is_some() {
                                format!("{origin}: --exclude leaves no files of '{label}' to copy")
                            } else {
                                format!("{origin}: '{label}' contains no files")
                            });
                        }
                        let at = if kept.is_empty() {
                            dest.clone()
                        } else {
                            join_virtual(dest, &kept)
                        };
                        src.mounted_at(&at)
                            .into_iter()
                            .map(|(p, n)| {
                                let mode = chmod.unwrap_or(n.mode);
                                (
                                    p,
                                    Arc::new(FileNode {
                                        bytes: n.bytes.clone(),
                                        mode,
                                        digest: n.digest.clone(),
                                        origin: origin.clone(),
                                    }),
                                )
                            })
                            .collect()
                    }
                };
                Ok(self.memo_store(key, files, Vec::new(), true))
            }
            Operation::Render {
                template,
                dest,
                each,
                escape,
                inline_svg,
                span,
                ..
            } => {
                let src = self.read_source(template, &origin, None)?;
                let source_key = if memoized {
                    self.source_key(template, &src)
                } else {
                    String::new()
                };
                let Source::File {
                    bytes,
                    inline,
                    label,
                    ..
                } = src
                else {
                    return Err(SitefileError::at(
                        E_TYPE,
                        format!(
                            "RENDER template '{}' is a directory, not a file",
                            src.label()
                        ),
                        template.span(),
                    ));
                };
                let text = decode_utf8(
                    &bytes,
                    &format!("template '{label}'"),
                    Some(template.span()),
                )?;
                let parts = match &inline {
                    Some((file, first)) => parse_render_template(&text, file, *first)?,
                    None => parse_render_template(&text, &label, 1)?,
                };
                // What the template names, the FOR list and the destination's variables (ARG/LET values are in op_id).
                // A template with `${file …}` (itself or in a fragment) reads the tree, so the input tree is part of its key.
                let scope = self.scope();
                let (deps, reads_files) = if memoized {
                    scope.render_deps(&parts)
                } else {
                    (BTreeSet::new(), false)
                };
                let template_key = if memoized {
                    memo_key(&[
                        "template",
                        &source_key,
                        if reads_files { &self.input_key } else { "" },
                    ])
                } else {
                    String::new()
                };
                let renames = self.renames.clone();
                let file_tree = FileTree {
                    tree,
                    renames: &renames,
                };
                let files_ctx: (&dyn crate::media::TemplateTree, bool) = (&file_tree, *inline_svg);
                let key = memoized.then(|| {
                    let mut names = deps.clone();
                    if let Some(e) = each {
                        names.insert(e.list.clone());
                        names.extend(operand_names(&[&e.dest]));
                    }
                    memo_key(&["render", op_id, &template_key, &self.names_key(names, None)])
                });
                if let Some(hit) = self.memo_lookup(&key) {
                    return Ok(hit);
                }
                let Some(each) = each else {
                    let out = scope.render(&parts, *escape, files_ctx)?;
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
                // BY / PAGE: the loop runs over groups or pages instead of the elements.
                let what = match &each.by {
                    Some(b) => format!("FOR {as_name} IN {list} BY {b}"),
                    None => format!("FOR {as_name} IN {list}"),
                };
                let groups = match &each.by {
                    Some(b) => Some(crate::pages::group_by(items, b, &what, span)?),
                    None => None,
                };
                if items.is_empty() && !(each.page.is_some() && groups.is_none()) {
                    (self.warn)(&format!(
                        "{origin}: FOR {as_name} IN {list} renders nothing: '{list}' is empty"
                    ));
                } else if groups.as_ref().is_some_and(|g| g.is_empty()) {
                    (self.warn)(&format!(
                        "{origin}: {what} renders nothing: no element has '{}'",
                        each.by.as_deref().unwrap_or_default()
                    ));
                }
                let dest_of = |item: &Value, op: &Operand| -> Result<String> {
                    let mut raw = String::new();
                    for part in &op.parts {
                        match part {
                            TemplatePart::Text(t) => raw.push_str(t),
                            TemplatePart::Var { name, path, span } => {
                                let v = if name == as_name {
                                    resolve_json_path(item, path, as_name, span)?
                                } else {
                                    scope.lookup(name, path, span)?
                                };
                                raw.push_str(&v.to_js_string());
                            }
                        }
                    }
                    let target = normalize_virtual(
                        &raw,
                        Some(&op.span),
                        &format!("RENDER destination for {as_name}"),
                    )?;
                    if target.trailing_slash {
                        return Err(SitefileError::at(
                            E_PATH,
                            format!(
                                "RENDER destination '{raw}' must be a file path, not a directory"
                            ),
                            &op.span,
                        ));
                    }
                    Ok(target.path)
                };
                // Each element with its destination. Pages get `url`, `prev` and `next` once every page's
                // destination is known.
                let mut elements: Vec<(Cow<'_, Value>, String)> = Vec::new();
                if let Some(pg) = &each.page {
                    let runs: Vec<(Option<String>, Vec<Value>)> = match &groups {
                        Some(gs) => gs
                            .iter()
                            .map(|g| {
                                (
                                    g["key"].as_str().map(str::to_string),
                                    g["items"].as_array().cloned().unwrap_or_default(),
                                )
                            })
                            .collect(),
                        None => vec![(None, items.clone())],
                    };
                    for pages in crate::pages::paginate(&runs, pg.size) {
                        let mut paths = Vec::with_capacity(pages.len());
                        for m in &pages {
                            let v = Value::Object(m.clone());
                            let op = match (&pg.first, m["number"].as_u64()) {
                                (Some(f), Some(1)) => f,
                                _ => dest_op,
                            };
                            paths.push(dest_of(&v, op)?);
                        }
                        let urls: Vec<String> = paths.iter().map(|p| page_url("", p)).collect();
                        for (i, (mut m, path)) in pages.into_iter().zip(paths).enumerate() {
                            m.insert("url".into(), json!(urls[i]));
                            m.insert(
                                "prev".into(),
                                json!(if i > 0 {
                                    urls[i - 1].clone()
                                } else {
                                    String::new()
                                }),
                            );
                            m.insert(
                                "next".into(),
                                json!(urls.get(i + 1).cloned().unwrap_or_default()),
                            );
                            elements.push((Cow::Owned(Value::Object(m)), path));
                        }
                    }
                } else {
                    match &groups {
                        Some(gs) => {
                            for g in gs {
                                let path = dest_of(g, dest_op)?;
                                elements.push((Cow::Owned(g.clone()), path));
                            }
                        }
                        None => {
                            for item in items {
                                let path = dest_of(item, dest_op)?;
                                elements.push((Cow::Borrowed(item), path));
                            }
                        }
                    }
                }
                // An element's page depends on the template, the element and the imports the template
                // names (other than the loop variable), so editing one document re-renders one page.
                let refs_key = if memoized {
                    self.names_key(deps, Some(as_name))
                } else {
                    String::new()
                };
                let children: Mutex<Vec<String>> = Mutex::new(Vec::new());
                let mut files: Files = Vec::new();
                for (item, target_path) in &elements {
                    let item: &Value = item;
                    let item_scope = scope.with_item(as_name, item);
                    let page = || -> Result<Node> {
                        let out = item_scope.render(&parts, *escape, files_ctx)?;
                        Ok(make_file(out.into_bytes(), origin.clone(), 0o644))
                    };
                    let node = match self.memo {
                        Some(memo) => {
                            let item_key = memo_key(&[
                                "item",
                                op_id,
                                &template_key,
                                &refs_key,
                                &value_key(item),
                            ]);
                            memoize(memo, &item_key, Some(&children), |_| page())?
                        }
                        None => page()?,
                    };
                    files.push((target_path.clone(), node));
                }
                Ok(self.memo_store(key, files, children.into_inner().unwrap_or_default(), true))
            }
        }
    }

    fn feed_files(&mut self, op: &Operation) -> Result<Files> {
        let Operation::Feed {
            list,
            base,
            link,
            dest,
            title,
            span,
            ..
        } = op
        else {
            unreachable!()
        };
        let origin = format!("{}:{} FEED", self.label, span.line);
        let scope = self.scope();
        let items = scope.value_of(list, &[], span, false)?;
        let Some(Value::Array(items)) = items.as_deref() else {
            return Err(SitefileError::at(
                E_TYPE,
                format!("FEED {list}: '{list}' is not a list (import a directory AS markdown)"),
                span,
            ));
        };
        if items.is_empty() {
            (self.warn)(&format!(
                "{origin}: '{list}' is empty, so the feed has no entries"
            ));
        }
        let mut entries = Vec::new();
        for (i, item) in items.iter().enumerate() {
            let Value::Object(doc) = item else {
                return Err(SitefileError::at(
                    E_TYPE,
                    format!(
                        "FEED {list}: element {} is not a document (an object with title and date)",
                        i + 1
                    ),
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
                    Some(v @ (Value::String(_) | Value::Number(_) | Value::Bool(_))) => {
                        Ok(Some(js_string_of(v)))
                    }
                    Some(_) => Err(SitefileError::at(
                        E_TYPE,
                        format!("FEED {list}: '{key}' of document '{name}' is not text"),
                        span,
                    )),
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
            let path = if l.path == "/" || !l.trailing_slash {
                l.path
            } else {
                format!("{}/", l.path)
            };
            let author = text("author")?.filter(|a| !a.is_empty());
            let summary = text("excerpt")?.filter(|s| !s.is_empty());
            let content = text("content")?;
            entries.push(FeedEntry {
                title,
                url: page_url(base, &path),
                date,
                author,
                summary,
                content,
            });
        }
        let xml = atom_xml(title, &format!("{base}/"), &page_url(base, dest), &entries);
        Ok(vec![(
            dest.clone(),
            make_file(xml.into_bytes(), origin, 0o644),
        )])
    }

    fn produce_transform(&mut self, op: &Operation, op_id: &str) -> Result<Produced> {
        let Operation::Transform {
            source,
            plugin,
            dest,
            options,
            each,
            args,
            include,
            exclude,
            span,
            ..
        } = op
        else {
            unreachable!()
        };
        let origin = format!("{}:{} TRANSFORM", self.label, span.line);
        let scope = self.scope();
        let list_value;
        let items: Vec<Option<&Value>> = match each {
            Some(e) => {
                list_value = scope.value_of(&e.list, &[], span, false)?;
                let Some(Value::Array(list)) = list_value.as_deref() else {
                    return Err(SitefileError::at(
                        E_TYPE,
                        format!(
                            "FOR {} IN {}: '{}' is not a list",
                            e.as_name, e.list, e.list
                        ),
                        span,
                    ));
                };
                if list.is_empty() {
                    (self.warn)(&format!(
                        "{origin}: FOR {} IN {} produces nothing: the list is empty",
                        e.as_name, e.list
                    ));
                }
                list.iter().map(Some).collect()
            }
            None => vec![None],
        };

        // The source is the same for every element; read it once.
        let mut input: Option<PluginInput> = None;
        let mut data: Option<Value> = None;
        let mut src_key = String::new();
        // A directory source: the plugin runs once per file (--include/--exclude pick them).
        let mut dir: Option<(Tree, String)> = None;
        let filtered = !include.is_empty() || !exclude.is_empty();
        if let Some(src_ref) = source {
            let value_import = match src_ref {
                SourceRef::Import { name, path, span } => match self.resources.get(name) {
                    Some(Resource::Value { value, .. }) => {
                        if path != "/" {
                            return Err(SitefileError::at(
                                E_PATH,
                                format!("import '{name}' is a value; use '{name}:'"),
                                span,
                            ));
                        }
                        Some(value.clone())
                    }
                    _ => None,
                },
                _ => None,
            };
            if let Some(v) = value_import {
                if let (true, SourceRef::Import { name, span, .. }) = (filtered, src_ref) {
                    return Err(SitefileError::at(E_TYPE, format!("--include and --exclude filter a directory, but '{name}:' is a value import"), span));
                }
                data = Some(v);
            } else {
                let rules = (!exclude.is_empty()).then(|| IgnoreRules::from_patterns(exclude));
                let src = self.read_source(src_ref, &origin, rules.as_ref())?;
                if self.memo.is_some() {
                    src_key = self.source_key(src_ref, &src);
                }
                match src {
                    Source::Tree { tree, label, .. } => {
                        if each.is_some() {
                            return Err(SitefileError::at(
                                E_TYPE,
                                format!("TRANSFORM source '{label}' is a directory, which runs the plugin once per file and cannot be combined with FOR"),
                                src_ref.span(),
                            ));
                        }
                        dir = Some((including(tree, include)?, label));
                    }
                    Source::File {
                        bytes,
                        basename,
                        label,
                        ..
                    } => {
                        if filtered {
                            return Err(SitefileError::at(E_TYPE, format!("--include and --exclude filter a directory, but '{label}' is a file"), src_ref.span()));
                        }
                        let name = if basename.is_empty() {
                            label.clone()
                        } else {
                            basename
                        };
                        input = Some(PluginInput::new(label, name, bytes.to_vec()));
                    }
                }
            }
        }

        let tool = self.tools.get(plugin).cloned();
        if let (Some(_), None, None, Some(s)) = (&tool, &input, &dir, source) {
            return Err(SitefileError::at(
                E_TYPE,
                format!(
                    "TRANSFORM … WITH {plugin}: a TOOL needs a file source, not a value import"
                ),
                s.span(),
            ));
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
            (Some(t), _) => Some(format!(
                "tool\0{}\0{}\0{}",
                t.path.display(),
                t.size,
                t.mtime_ms
            )),
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
            memo_key(&[
                "transform",
                op_id,
                &src_key,
                &code,
                &self.names_key(names, None),
            ])
        });
        if let Some((tree, label)) = dir {
            if let Some(hit) = self.memo_lookup(&key) {
                return Ok(hit);
            }
            let file_base = memo.map(|_| {
                memo_key(&[
                    "transform-file",
                    op_id,
                    &code,
                    &self.names_key(operand_refs.clone(), None),
                ])
            });
            let mut children: Vec<String> = Vec::new();
            let ctx = DirTransform {
                tree: &tree,
                label: &label,
                file_base,
                tool: tool.as_ref(),
                plugin_digest: plugin_digest.as_deref(),
                memo,
            };
            let files = self.transform_files(op, &ctx, &mut children)?;
            let store = tool.is_some() || self.plugins.cacheable(plugin);
            return Ok(self.memo_store(key, files, children, store));
        }
        // One FOR element's files: the same request gives the same result (the list itself is not read).
        let item_base = match (memo, each) {
            (Some(_), Some(e)) => Some(memo_key(&[
                "transform-item",
                op_id,
                &src_key,
                &code,
                &self.names_key(operand_refs, Some(&e.as_name)),
            ])),
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
            if star_name(&raw_dest).is_some() {
                return Err(SitefileError::at(
                    E_TYPE,
                    format!("TRANSFORM destination '{raw_dest}': a '*' name stands for each file of a directory source, and this TRANSFORM's source is not a directory"),
                    &dest.span,
                ));
            }
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
                    req.insert(
                        "input".into(),
                        json!({ "path": i.path, "name": i.name, "digest": dg }),
                    );
                }
                if let Some(dt) = &data {
                    req.insert("data".into(), dt.clone());
                }
                if let Some(it) = item {
                    req.insert("item".into(), it.clone());
                }
                Some(cache_key(
                    &json!({ "code": code, "request": Value::Object(req) }),
                ))
            } else {
                None
            };
            let call = begin(
                self.profiler,
                || format!("TRANSFORM {plugin} {}", d.path),
                "transform",
                || {
                    vec![
                        ("plugin", json!(plugin)),
                        ("tool", json!(tool.is_some())),
                        ("dest", json!(d.path)),
                    ]
                },
            );
            let mut result: Option<PluginResult> = match (&cached, self.cache.as_mut()) {
                (Some(k), Some(c)) => c.get_for(k, Some(plugin)),
                _ => None,
            };
            let cache_state = if cached.is_none() {
                "off"
            } else if result.is_some() {
                "hit"
            } else {
                "miss"
            };
            if result.is_none() {
                check_cancel(self.cancel)?;
                let r = if let Some(t) = &tool {
                    self.stats.tool_runs += 1;
                    let call = ToolCall {
                        args: argv.clone().unwrap_or_default(),
                        dest: d.path.clone(),
                        options: opts.clone(),
                        input: input.as_ref().map(|i| ToolInput {
                            name: i.name.clone(),
                            bytes: i.bytes.clone(),
                        }),
                    };
                    // The tool's stderr, like a TRANSFORM cache hit, is not replayed by a memo hit.
                    let warn = self.unrecorded.clone();
                    run_tool(
                        t,
                        &call,
                        &self.tool_policy,
                        &move |l: &str| warn(l),
                        Some(span),
                    )?
                } else {
                    self.plugins.call(plugin, &request, Some(span))?
                };
                if let Some(k) = &cached {
                    if tool.is_some() || self.plugins.cacheable(plugin) {
                        if let Some(c) = self.cache.as_mut() {
                            c.put_for(k, Some(plugin), &r);
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
                        let p = normalize_virtual(
                            &format!("{base}/{rel}"),
                            Some(&dest.span),
                            &format!("file '{rel}' returned by PLUGIN {plugin}"),
                        )?
                        .path;
                        if rel.contains("..") || rel.starts_with('/') {
                            return Err(SitefileError::at(
                                E_PLUGIN,
                                format!("PLUGIN {plugin} returned an unsafe path '{rel}'"),
                                span,
                            ));
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

    /// TRANSFORM over a directory: one plugin (or tool) call per file, in path order. Each call
    /// gets its own `input` and destination (see `file_dest`), is cached and memoized per file,
    /// and calls run up to `transform_jobs()` at a time (a JavaScript plugin, one process, takes
    /// one at a time). Files, tool output, profile events and errors come in path order whatever
    /// order the calls finish in.
    fn transform_files(
        &mut self,
        op: &Operation,
        ctx: &DirTransform,
        children: &mut Vec<String>,
    ) -> Result<Files> {
        let Operation::Transform {
            plugin,
            dest,
            options,
            args,
            include,
            exclude,
            span,
            ..
        } = op
        else {
            unreachable!()
        };
        let line = format!("{}:{} TRANSFORM", self.label, span.line);
        if ctx.tree.is_empty() {
            (self.warn)(&if !include.is_empty() || !exclude.is_empty() {
                format!(
                    "{line}: --include/--exclude leave no files of '{}' to transform",
                    ctx.label
                )
            } else {
                format!("{line}: '{}' contains no files", ctx.label)
            });
        }
        let scope = self.scope();
        let raw_dest = scope.interpolate(dest)?;
        normalize_virtual(&raw_dest, Some(&dest.span), "TRANSFORM destination")?;
        let dir_dest = if raw_dest.ends_with('/') {
            raw_dest.clone()
        } else {
            format!("{raw_dest}/")
        };
        if ctx.tool.is_some() && star_name(&raw_dest).is_none() {
            return Err(SitefileError::at(
                E_PLUGIN,
                format!(
                    "TOOL {plugin} writes one file per input, but '{raw_dest}' is a directory destination: use '{dir_dest}*' to keep each file's name, or '{dir_dest}*.ext' to change its extension"
                ),
                &dest.span,
            ));
        }
        // Object.create(null) + assignment: a repeated key keeps its first position, the last value.
        let mut opts: Vec<(String, String)> = Vec::new();
        for o in options {
            let v = scope.interpolate(&o.value)?;
            match opts.iter_mut().find(|(k, _)| *k == o.key) {
                Some(slot) => slot.1 = v,
                None => opts.push((o.key.clone(), v)),
            }
        }
        let argv: Vec<String> = match ctx.tool {
            Some(_) => args
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .map(|a| scope.interpolate(a))
                .collect::<Result<_>>()?,
            None => Vec::new(),
        };
        let store = |host: &PluginHost| ctx.tool.is_some() || host.cacheable(plugin);

        struct Job<'p> {
            index: usize,
            label: String,
            origin: String,
            raw_dest: String,
            path: String,
            trailing_slash: bool,
            request: PluginRequest,
            cached: Option<String>,
            memo_key: Option<String>,
            span: crate::profile::Span<'p>,
            logs: Mutex<Vec<String>>,
            result: Option<PluginResult>,
            cache: &'static str,
        }
        let entries: Vec<(String, Node)> = ctx
            .tree
            .entries()
            .map(|(p, n)| (p.clone(), n.clone()))
            .collect();
        let mut done: Vec<Option<Files>> = vec![None; entries.len()];
        let mut jobs: Vec<Job> = Vec::new();
        for (index, (p, node)) in entries.iter().enumerate() {
            let rel = &p[1..];
            let label = format!(
                "{}{rel}",
                if ctx.label.ends_with('/') {
                    ctx.label.to_string()
                } else {
                    format!("{}/", ctx.label)
                }
            );
            let file_key = ctx
                .file_base
                .as_ref()
                .map(|b| memo_key(&[b, rel, &node.digest]));
            if let (Some(m), Some(k)) = (ctx.memo, &file_key) {
                children.push(k.clone());
                if let Some(reused) = lock_memo(m).get::<Files>(k) {
                    done[index] = Some(reused);
                    continue;
                }
            }
            let per_file = file_dest(&raw_dest, rel);
            let d = normalize_virtual(&per_file, Some(&dest.span), "TRANSFORM destination")?;
            let input = PluginInput::new(
                label.clone(),
                basename_virtual(p).to_string(),
                node.bytes.to_vec(),
            );
            let cached = if self.cache.is_some()
                && (ctx.tool.is_some() || ctx.plugin_digest.is_some())
            {
                let code = match ctx.tool {
                    Some(t) => json!({
                        "tool": { "path": t.path.to_string_lossy(), "size": t.size, "mtimeMs": t.mtime_ms },
                        "args": argv,
                    }),
                    None => json!({ "plugin": ctx.plugin_digest }),
                };
                let mut req = Map::new();
                req.insert("dest".into(), Value::String(d.path.clone()));
                let mut om = Map::new();
                for (k, v) in &opts {
                    om.insert(k.clone(), Value::String(v.clone()));
                }
                req.insert("options".into(), Value::Object(om));
                req.insert("input".into(), json!({ "path": input.path, "name": input.name, "digest": sha256(&input.bytes) }));
                Some(cache_key(
                    &json!({ "code": code, "request": Value::Object(req) }),
                ))
            } else {
                None
            };
            let ev_args = vec![
                ("plugin", json!(plugin)),
                ("tool", json!(ctx.tool.is_some())),
                ("dest", json!(d.path)),
                ("input", json!(label)),
            ];
            let ev = match self.profiler {
                Some(pr) => pr.reserve(
                    format!("TRANSFORM {plugin} {}", d.path),
                    "transform",
                    ev_args,
                    1,
                ),
                None => crate::profile::Span::none(),
            };
            let mut job = Job {
                index,
                origin: format!("{line} {label}"),
                label,
                raw_dest: per_file,
                path: d.path.clone(),
                trailing_slash: d.trailing_slash,
                request: PluginRequest {
                    dest: d.path.clone(),
                    options: opts.clone(),
                    input: Some(input),
                    data: None,
                    item: None,
                },
                cache: if cached.is_some() { "miss" } else { "off" },
                cached,
                memo_key: file_key,
                span: ev,
                logs: Mutex::new(Vec::new()),
                result: None,
            };
            if let (Some(k), Some(c)) = (&job.cached, self.cache.as_mut()) {
                if let Some(hit) = c.get_for(k, Some(plugin)) {
                    job.span.start();
                    job.span.end(vec![("cache", json!("hit"))]);
                    job.result = Some(hit);
                    job.cache = "hit";
                }
            }
            jobs.push(job);
        }

        // The misses, a few at a time (a JavaScript plugin's one process takes one call at a time).
        let misses: Vec<usize> = (0..jobs.len())
            .filter(|&i| jobs[i].result.is_none())
            .collect();
        let settled: Vec<Option<Result<PluginResult>>> = if let Some(t) = ctx.tool {
            let policy = &self.tool_policy;
            let jobs_ref = &jobs;
            let cancel = self.cancel;
            let out = run_bounded(misses.len(), transform_jobs(), |k| {
                check_cancel(cancel)?;
                let job = &jobs_ref[misses[k]];
                job.span.start();
                let input = job.request.input.as_ref().map(|i| ToolInput {
                    name: i.name.clone(),
                    bytes: i.bytes.clone(),
                });
                let call = ToolCall {
                    args: argv.clone(),
                    dest: job.path.clone(),
                    options: opts.clone(),
                    input,
                };
                let log = |l: &str| {
                    job.logs
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(l.to_string())
                };
                let r = run_tool(t, &call, policy, &log, Some(span));
                if r.is_ok() {
                    job.span.end(vec![("cache", json!(job.cache))]);
                }
                r
            });
            self.stats.tool_runs += out.iter().filter(|r| r.is_some()).count() as u64;
            out
        } else if let Some(w) = self.plugins.shared_wasm(plugin, Some(span))? {
            let jobs_ref = &jobs;
            let cancel = self.cancel;
            let out = run_bounded(misses.len(), transform_jobs(), |k| {
                check_cancel(cancel)?;
                let job = &jobs_ref[misses[k]];
                job.span.start();
                let r = w.call_prepared(&job.request, Some(span));
                if r.is_ok() {
                    job.span.end(vec![("cache", json!(job.cache))]);
                }
                r
            });
            self.plugins.stats.calls += out.iter().filter(|r| r.is_some()).count() as u64;
            out
        } else {
            let mut out: Vec<Option<Result<PluginResult>>> = Vec::new();
            for &i in &misses {
                let job = &jobs[i];
                job.span.start();
                let r = check_cancel(self.cancel)
                    .and_then(|_| self.plugins.call(plugin, &job.request, Some(span)));
                let failed = r.is_err();
                if !failed {
                    job.span.end(vec![("cache", json!(job.cache))]);
                }
                out.push(Some(r));
                if failed {
                    break;
                }
            }
            out.resize_with(misses.len(), || None);
            out
        };
        let mut errors: HashMap<usize, SitefileError> = HashMap::new();
        for (k, r) in settled.into_iter().enumerate() {
            match r {
                Some(Ok(v)) => jobs[misses[k]].result = Some(v),
                Some(Err(e)) => {
                    errors.insert(misses[k], e);
                }
                None => {}
            }
        }

        // In path order: tool output, the first failure, cache writes and the files.
        for (ji, job) in jobs.iter_mut().enumerate() {
            // The tool's stderr, like a TRANSFORM cache hit, is not replayed by a memo hit.
            for l in job
                .logs
                .get_mut()
                .unwrap_or_else(|e| e.into_inner())
                .drain(..)
            {
                (self.unrecorded)(&l);
            }
            if let Some(e) = errors.remove(&ji) {
                if e.code == E_CANCELLED {
                    return Err(e);
                }
                return Err(SitefileError {
                    detail: format!("{}: {}", job.label, e.detail),
                    ..e
                });
            }
            let Some(result) = job.result.take() else {
                unreachable!("every job before the first failure ran")
            };
            if job.cache == "miss" && store(&self.plugins) {
                if let (Some(k), Some(c)) = (&job.cached, self.cache.as_mut()) {
                    c.put_for(k, Some(plugin), &result);
                }
            }
            let mut files: Files = Vec::new();
            match result {
                PluginResult::File(bytes) => {
                    if job.trailing_slash {
                        return Err(SitefileError::at(
                            E_PLUGIN,
                            format!(
                                "{}: PLUGIN {plugin} returned one file, but '{}' is a directory destination (use '{dir_dest}*' or '{dir_dest}*.ext' for one file per input)",
                                job.label, job.raw_dest
                            ),
                            &dest.span,
                        ));
                    }
                    files.push((
                        job.path.clone(),
                        make_file(bytes, job.origin.clone(), 0o644),
                    ));
                }
                PluginResult::Files(list) => {
                    for (rel, bytes) in list {
                        let base = if job.path == "/" {
                            ""
                        } else {
                            job.path.as_str()
                        };
                        let p = normalize_virtual(
                            &format!("{base}/{rel}"),
                            Some(&dest.span),
                            &format!("file '{rel}' returned by PLUGIN {plugin}"),
                        )?
                        .path;
                        if rel.contains("..") || rel.starts_with('/') {
                            return Err(SitefileError::at(
                                E_PLUGIN,
                                format!(
                                    "{}: PLUGIN {plugin} returned an unsafe path '{rel}'",
                                    job.label
                                ),
                                span,
                            ));
                        }
                        files.push((p, make_file(bytes, job.origin.clone(), 0o644)));
                    }
                }
            }
            if let (Some(m), Some(k)) = (ctx.memo, &job.memo_key) {
                if store(&self.plugins) {
                    lock_memo(m).set(k.clone(), files.clone(), Vec::new());
                }
            }
            done[job.index] = Some(files);
        }

        // Two inputs may write the same path only with the same content.
        let mut out: Files = Vec::new();
        let mut owner: HashMap<String, Node> = HashMap::new();
        for (p, node) in done.into_iter().flatten().flatten() {
            if let Some(prior) = owner.get(&p) {
                if prior.digest == node.digest {
                    continue;
                }
                return Err(SitefileError::at(
                    E_CONFLICT,
                    format!(
                        "'{p}' is written for two inputs with different content ({} and {}); rename one, or leave one out with --include/--exclude",
                        prior.origin, node.origin
                    ),
                    span,
                ));
            }
            owner.insert(p.clone(), node.clone());
            out.push((p, node));
        }
        Ok(out)
    }
}

/// What `transform_files` needs besides the operation: the source directory and its label, the
/// per-file memo key base, the tool or the plugin's code digest, and the memo.
struct DirTransform<'t> {
    tree: &'t Tree,
    label: &'t str,
    file_base: Option<String>,
    tool: Option<&'t ResolvedTool>,
    plugin_digest: Option<&'t str>,
    memo: Option<&'t Mutex<BuildMemo>>,
}

/// A directory source with only the files an `--include` pattern matches (relative to its root).
fn including(tree: Tree, include: &[String]) -> Result<Tree> {
    if include.is_empty() {
        return Ok(tree);
    }
    let rules = IgnoreRules::from_patterns(include);
    Tree::from_files(
        tree.entries()
            .filter(|(p, _)| rules.excludes(&p[1..]))
            .map(|(p, n)| (p.clone(), n.clone())),
    )
}

/// The last segment of a TRANSFORM destination when it names each file of a directory source:
/// `*` (the file's own name) or `*.ext` (its name with the extension replaced).
pub(crate) fn star_name(dest: &str) -> Option<&str> {
    let last = &dest[dest.rfind('/').map_or(0, |i| i + 1)..];
    let ext_ok = last.len() > 2 && last.starts_with("*.") && !last[2..].contains('*');
    (last == "*" || ext_ok).then_some(last)
}

/// Where one file of a directory TRANSFORM goes: `rel` is its path inside the source directory.
/// `TO /out/*` keeps its relative path, `TO /out/*.ext` replaces its extension, and any other
/// destination is the directory `<dest>/<relative dir>/` (the plugin names the files).
pub(crate) fn file_dest(dest: &str, rel: &str) -> String {
    let base = &dest[..dest.rfind('/').map_or(0, |i| i + 1)];
    let cut = rel.rfind('/').map_or(0, |i| i + 1);
    let (rel_dir, name) = (&rel[..cut], &rel[cut..]);
    match star_name(dest) {
        Some("*") => format!("{base}{rel}"),
        Some(star) => {
            let stem = match name.rfind('.') {
                Some(dot) if dot > 0 => &name[..dot],
                _ => name,
            };
            format!("{base}{rel_dir}{stem}{}", &star[1..])
        }
        None => format!(
            "{}{rel_dir}",
            if dest.ends_with('/') {
                dest.to_string()
            } else {
                format!("{dest}/")
            }
        ),
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
        | Operation::Sitemap { overwrite, .. }
        | Operation::Fingerprint { overwrite, .. }
        | Operation::Precompress { overwrite, .. }
        | Operation::TextFile { overwrite, .. }
        | Operation::Precache { overwrite, .. } => *overwrite,
        Operation::CheckLinks { .. } | Operation::Canonical { .. } => false,
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

/// Whether a template has a `${file …}` (it then reads the tree at its RENDER).
fn uses_files(nodes: &[TemplateNode]) -> bool {
    nodes.iter().any(|n| match n {
        TemplateNode::File { .. } => true,
        TemplateNode::Each { body, empty, .. } => uses_files(body) || uses_files(empty),
        TemplateNode::If {
            then, otherwise, ..
        } => uses_files(then) || uses_files(otherwise),
        _ => false,
    })
}

impl crate::media::TemplateTree for Tree {
    fn get(&self, path: &str) -> Option<&[u8]> {
        Tree::get(self, path).map(|n| &n.bytes[..])
    }
    fn is_directory(&self, path: &str) -> bool {
        Tree::is_directory(self, path)
    }
}

/// The tree a RENDER's `${file …}` reads, with the stage's FINGERPRINT renames.
struct FileTree<'a> {
    tree: &'a Tree,
    renames: &'a BTreeMap<String, String>,
}

impl crate::media::TemplateTree for FileTree<'_> {
    fn get(&self, path: &str) -> Option<&[u8]> {
        Tree::get(self.tree, path).map(|n| &n.bytes[..])
    }
    fn is_directory(&self, path: &str) -> bool {
        Tree::is_directory(self.tree, path)
    }
    fn renamed(&self, path: &str) -> Option<String> {
        self.renames.get(path).cloned()
    }
}

/// Top-level names a template reads (variables, #each lists, #if conditions).
pub(crate) fn template_names(nodes: &[TemplateNode]) -> BTreeSet<String> {
    fn walk_nodes(nodes: &[TemplateNode], out: &mut BTreeSet<String>) {
        for n in nodes {
            match n {
                TemplateNode::Text(_) => {}
                TemplateNode::Var { name, .. } => {
                    out.insert(name.clone());
                }
                TemplateNode::File { path, attrs, .. } => {
                    for (name, _, _) in crate::render::file_refs(path, attrs) {
                        out.insert(name.to_string());
                    }
                }
                TemplateNode::Each {
                    list, body, empty, ..
                } => {
                    out.insert(list.name.clone());
                    walk_nodes(body, out);
                    walk_nodes(empty, out);
                }
                TemplateNode::If {
                    cond,
                    then,
                    otherwise,
                    ..
                } => {
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
        assert_eq!(
            relative_path(Path::new("/a/b"), Path::new("/a/b/c/d")),
            "c/d"
        );
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
        let dir = std::env::temp_dir().join(format!(
            "sitefile-pins-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Sitefile"), sitefile).unwrap();
        if let Some(l) = lock {
            std::fs::write(dir.join("Sitefile.lock"), l).unwrap();
        }
        let mut o = BuildOptions {
            context_dir: dir.display().to_string(),
            strict_pins: true,
            ..Default::default()
        };
        // Nothing listens on port 9: a fetch fails with E_FETCH, so E_UNPINNED proves the check ran first.
        o.fetch_policy.allow_private_network = true;
        (dir, o)
    }

    const URL: &str = "https://127.0.0.1:9/lib.js";
    const PIN: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000000";

    #[test]
    fn strict_pins_refuses_unpinned_imports_before_fetching() {
        let (dir, o) = strict_ctx(
            &format!("FROM scratch\nIMPORT p FROM ./\nIMPORT lib FROM {URL}\nEXPORT /\n"),
            None,
        );
        let e = build(&o).err().unwrap();
        assert_eq!(e.code, E_UNPINNED);
        assert_eq!(e.span.as_ref().map(|s| s.line), Some(3));
        assert!(
            e.detail.starts_with(&format!(
                "IMPORT lib: '{URL}' is not pinned (--strict-pins); add INTEGRITY sha256:<hex>"
            )),
            "{}",
            e.detail
        );
        // INTEGRITY is a pin: the build gets as far as fetching
        let (_, o) = strict_ctx(
            &format!("FROM scratch\nIMPORT lib FROM {URL} INTEGRITY {PIN}\nEXPORT /\n"),
            None,
        );
        assert_eq!(build(&o).err().unwrap().code, E_FETCH);
        // so is a lockfile entry, unless --no-lockfile
        let lock = format!(
            r#"{{"version":1,"resources":{{"{URL}":{{"integrity":"{PIN}","size":1,"resolved":"{URL}"}}}}}}"#
        );
        let (_, o) = strict_ctx(
            &format!("FROM scratch\nIMPORT lib FROM {URL}\nEXPORT /\n"),
            Some(&lock),
        );
        assert_eq!(build(&o).err().unwrap().code, E_FETCH);
        assert!(check_plan_pins(&o, &plan_build(&o).unwrap()).is_ok());
        let o = BuildOptions {
            lockfile: LockfileSetting::Disabled,
            ..o
        };
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
        let skip = BuildOptions {
            skip_base: true,
            ..o.clone()
        };
        assert!(check_plan_pins(&skip, &plan_build(&skip).unwrap()).is_ok());
        // a lockfile entry or a digest pins it; --base is checked like the Sitefile's BASE
        let lock = format!(
            r#"{{"version":1,"resources":{{}},"images":{{"127.0.0.1:9/lib/base:1":{{"digest":"{PIN}"}}}}}}"#
        );
        let (_, o) = strict_ctx(tagged, Some(&lock));
        assert!(check_plan_pins(&o, &plan_build(&o).unwrap()).is_ok());
        let other = BuildOptions {
            base_image: Some("127.0.0.1:9/lib/base:2".into()),
            ..o
        };
        assert_eq!(
            check_plan_pins(&other, &plan_build(&other).unwrap())
                .unwrap_err()
                .code,
            E_UNPINNED
        );
        let (_, o) = strict_ctx(
            &format!("FROM scratch\nEXPORT / AS oci BASE 127.0.0.1:9/lib/base:1@{PIN}\n"),
            None,
        );
        assert!(check_plan_pins(&o, &plan_build(&o).unwrap()).is_ok());
    }

    #[test]
    fn map_limit_keeps_order_and_first_error() {
        let items: Vec<u32> = (0..20).collect();
        let r = map_limit(&items, 4, |i| Ok(i * 2)).unwrap();
        assert_eq!(r, (0..20).map(|i| i * 2).collect::<Vec<_>>());
        let e = map_limit(&items, 4, |i| {
            if *i == 3 || *i == 7 {
                Err(SitefileError::plain(E_FETCH, format!("{i}")))
            } else {
                Ok(*i)
            }
        })
        .unwrap_err();
        assert_eq!(e.detail, "3");
    }
}

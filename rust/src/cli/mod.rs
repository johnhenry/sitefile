//! The `sitefile` command line (port of `src/cli.ts` and `src/cli/*.ts`):
//! the same commands, flags, help text, output lines and exit codes
//! (0 ok, 1 build error, 2 usage, 70 internal error).

pub mod args;
pub mod bake;
pub mod cache;
pub mod common;
pub mod git_context;
pub mod images;
pub mod outdated;
pub mod registry;
pub mod servers;
pub mod vendor;

use crate::build_args::collect_build_args;
use crate::error::{ErrorCode, Result, SitefileError};
use crate::executor::{
    build, build_to_directory, check_plan_pins, join_path, plan_build, relative_path,
    sitefile_compression, BuildOptions, BuildResult, Callback, ExportTarget, LockfileSetting,
};
use crate::export::attest::AttestRequest;
use crate::export::formats::{compression_from_extension, default_out_name, format_from_extension};
use crate::export::oci::parse_labels;
use crate::language::ast::{ExportFormat, OciCompression};
use crate::profile::Profiler;
use crate::refs::{parse_local_ref, ref_key};
use crate::resources::types::FetchPolicy;
use crate::secrets::{load_secrets, process_env};
use crate::store::short_id;
use args::{flag, flag_s, parse, string, string_s, strings, strings_s, OptSpec};
use common::{
    err, is_safe_integer, js_to_number, out, pretty_json, report, run_command, usage_error, Command,
};
use serde_json::json;
use std::sync::Arc;

pub const VERSION: &str = crate::export::attest::SITEFILE_VERSION;

const EXTRA_HELP: &str = "
Images (local store: $SITEFILE_HOME or ~/.sitefile):
  sitefile build [context] -t name[:tag]           also store the result as an image (repeatable -t)
  sitefile images [--json] [--digests] [-q]        list images (alias: image ls)
  sitefile image inspect <ref> [--json]            show an image manifest
  sitefile image prune                             delete unreferenced manifests and blobs
  sitefile image diff <a> <b>                      added/removed/changed paths
  sitefile tag <src> <dst>                         add a name:tag
  sitefile rmi [-f] <ref|id>...                    remove tags / images
  sitefile save <ref> -o out[.zip|.tar|.tar.gz|.wbn]   export an image
  sitefile push <registry/name[:tag]>              push a (tagged) image to an OCI registry
  sitefile pull <[registry/]name[:tag][@digest]>   pull an image (a bare name means Docker Hub)

Variants:
  sitefile bake [targets...] [-f file] [--print]   build several targets (matrix, groups) from
                                                   sitefile.bake.yaml; --print shows the plan

Offline builds:
  sitefile vendor [context] [--vendor-dir dir]     download remote imports and the BASE image into
                                                   vendor/sitefile; builds then read them from there

Pins:
  sitefile outdated [context] [--json]             compare remote imports and the BASE image with
                                                   their pins (never changes anything)

Build cache (TRANSFORM results: $SITEFILE_HOME/cache/transform):
  sitefile cache info [--json]                     location, entries, size
  sitefile cache prune [--older-than <days>]       delete cached results
  sitefile cache prune --max-size <size>           shrink evictable data (LRU) to <size>
  sitefile cache du [--json]                       size of every cache and the store (alias: cache ls)
  Evictable data over $SITEFILE_CACHE_MAX (default 1GiB, 0 = no limit) is deleted after each build.

Serving sites (detached servers are tracked by name):
  sitefile serve [--name n] [-p [host:]port] [-d] [--spa] <ref|./dir|file>
  sitefile serve --watch [-p [host:]port] [ctx]    build in memory, serve, rebuild + live reload
  sitefile servers [-a] [--json]                   list servers (alias: server ls)
  sitefile server stop|rm [-f]|restart|logs [-f] <name|id>

Editor support:
  sitefile lsp [--stdio]                           language server over stdio (Rust build only; docs/LSP.md)

Run 'sitefile <command> --help' for the options of a command.
";

fn help() -> String {
    let d = FetchPolicy::default();
    format!(
        "sitefile {VERSION} — Docker-inspired static artifact builder

Usage:
  sitefile build [context] [options]     build and export to --out (default: <context>/dist, or dist.<ext>)
  sitefile validate [context] [options]  parse, validate and evaluate variables (no IO)
  sitefile inspect [context] [options]   build in memory and list the exported files
  sitefile graph [context] [options]     print the dependency graph of imports, stages and operations
                                         (text, --json or --dot; no IO, like validate)
  sitefile why <path> [context] [opts]   build in memory and show which instructions wrote an exported
                                         file, from what inputs, and what overwrote it
  The context may be a Git URL, https://host/repo.git#ref:dir (fetched to a temporary directory;
  the default --out is then ./dist; see docs/CLI.md).

Options:
  -f, --file <path>            Sitefile path (default: <context>/Sitefile)
      --target <stage>         stop after the named stage and export its whole tree instead of
                               the EXPORT path (EXPORT's format still applies; no _redirects/_headers)
  -o, --out <path>             export destination (build only): a directory for 'directory',
                               a file for zip, tar, tgz, wbn (*.wbn.gz: a gzipped bundle)
  -t, --tag <name[:tag]>       also store the build as a local image (build only, repeatable);
                               with -t and no -o nothing is written to the context
      --format <fmt>           directory | zip | tar | tgz (alias tar.gz) | wbn | oci (build only).
                               oci writes an OCI archive for -o *.tar (e.g. site.oci.tar), else an
                               OCI image layout directory.
                               Default: the --out extension (.zip .tar .tar.gz .tgz .wbn .wbn.gz .oci.tar),
                               else the Sitefile's EXPORT ... AS <fmt>
      --oci-root <path>        directory inside an oci image layer (default /srv/www; overrides EXPORT ... AT)
      --compression <c>        gzip or none (build only; overrides EXPORT ... COMPRESSION): the oci site layer
                               (default gzip; none: plain tar), or the whole wbn (default none; gzip:
                               gzip(application/webbundle), also chosen by -o *.wbn.gz)
      --oci-compression <c>    the same as --compression, for oci only
      --label <key=value>      add an OCI image config label (repeatable)
      --git-labels             add org.opencontainers.image.source/revision from the git checkout
                               (off by default: they change the image ID on every commit)
      --provenance             attach a SLSA provenance attestation to the oci image (an in-toto
                               statement: Sitefile digest, build args, imports, BASE, builder)
      --sbom                   attach an SBOM attestation (CycloneDX: every file with its SHA-256);
                               both are OCI 1.1 referrers of the image (its ID does not change),
                               written into the oci export and kept with -t images for push
      --platform <os/arch>     oci image platform (default linux/amd64); with a BASE, the platform pulled
      --base <image>           build the oci image on <image> (runnable; overrides EXPORT ... AS oci BASE);
                               a bare name means Docker Hub. Pin it: nginx:alpine@sha256:...
      --insecure-registry <h>  allow plain http to registry host[:port] (repeatable; localhost always may)
      --base-url <url>         https URL ending in '/' the files are mounted at (wbn only);
                               overrides EXPORT ... BASE
      --build-arg <NAME=value> set an ARG (repeatable); --build-arg NAME reads $NAME
      --secret id=<id>,env=<VAR> | id=<id>,src=<file>
                               a build secret for IMPORT ... SECRET <id> AS HEADER <Name> | AS BEARER
                               (repeatable); sent only to that import's origin, never recorded
      --plugin-timeout <ms>    per-call time limit for TRANSFORM plugins and tools (default 60000)
      --allow-exec <name[=path]> allow a TOOL (external program) to run, resolved on PATH or at
                               the absolute path (repeatable); programs run with your privileges
      --allow-plugin <name>    allow a JavaScript PLUGIN to run (repeatable; wasm plugins need no flag)
      --allow-plugins          allow every JavaScript PLUGIN in the Sitefile
      --no-cache               do not read or write the TRANSFORM result cache
                               ($SITEFILE_HOME/cache/transform; see 'sitefile cache --help')
      --cache-from <ref>       also look up TRANSFORM results in this registry cache (build only,
                               repeatable, first match wins); results are fetched only on a hit
      --cache-to <ref>         after a successful build, push the TRANSFORM results it used to this
                               registry cache (build only, once); registry problems only warn
      --build-arg-file <path>  read ARGs from a .json, .yaml/.yml or .env file (repeatable;
                               later files win, --build-arg wins over files)
      --force                  replace a non-empty --out outside the context
      --frozen-lockfile        fail instead of changing Sitefile.lock
      --update-lock            re-record unpinned URLs whose content changed
      --strict-pins            fail when a remote IMPORT has no INTEGRITY and no lockfile entry, or the
                               BASE image is neither @sha256:-pinned nor locked (build, validate, inspect)
      --lint-disable <rules>   turn lint rules off (validate; comma-separated rule ids, or all; repeatable)
      --no-lockfile            do not read or write Sitefile.lock
      --vendor-dir <dir>       read remote imports and BASE blobs from this vendor directory (relative
                               to the context; default vendor/sitefile when it exists; see 'sitefile vendor')
      --allow-http             permit plain http:// imports
      --allow-private-network  permit loopback/private network addresses
      --allow-hardlinks        permit hard-linked files in the context (you assert they are safe)
      --max-download-bytes <n> per-resource size limit (default {})
      --timeout <ms>           per-resource fetch timeout (default {})
      --json                   machine-readable output (validate, inspect, graph, why)
      --dot                    Graphviz DOT output (graph only)
      --watch                  build, then rebuild when the context, the Sitefile or a
                               --build-arg-file changes (build only; Ctrl-C stops)
      --profile <file>         write a timing trace of the build (Chrome trace event JSON:
                               chrome://tracing, ui.perfetto.dev); --watch rewrites it per build
  -h, --help                   show this help
  -v, --version                show the version
{EXTRA_HELP}",
        d.max_bytes, d.timeout_ms
    )
}

const BUILD_OPTS: &[OptSpec] = &[
    string_s("file", 'f'),
    string("target"),
    string_s("out", 'o'),
    strings_s("tag", 't'),
    string("format"),
    string("base-url"),
    string("oci-root"),
    string("oci-compression"),
    string("compression"),
    strings("label"),
    flag("git-labels"),
    flag("provenance"),
    flag("sbom"),
    string("platform"),
    string("base"),
    strings("insecure-registry"),
    strings("build-arg"),
    strings("build-arg-file"),
    strings("secret"),
    string("plugin-timeout"),
    strings("allow-exec"),
    strings("allow-plugin"),
    flag("allow-plugins"),
    flag("no-cache"),
    strings("cache-from"),
    strings("cache-to"),
    flag("force"),
    flag("frozen-lockfile"),
    flag("update-lock"),
    flag("strict-pins"),
    strings("lint-disable"),
    flag("no-lockfile"),
    string("vendor-dir"),
    flag("allow-http"),
    flag("allow-private-network"),
    flag("allow-hardlinks"),
    string("max-download-bytes"),
    string("timeout"),
    flag("json"),
    flag("dot"),
    flag("watch"),
    string("profile"),
    flag_s("help", 'h'),
    flag_s("version", 'v'),
];

// ------------------------------------------------------------ store/server commands

const TOP: &[&Command] = &[
    &images::IMAGES,
    &images::TAG,
    &images::RMI,
    &images::SAVE,
    &servers::SERVE,
    &servers::PS,
    &registry::PUSH,
    &registry::PULL,
    &vendor::VENDOR,
    &outdated::OUTDATED,
    &bake::BAKE,
];

struct Group {
    name: &'static str,
    usage: &'static str,
    subs: &'static [(&'static str, &'static Command)],
}

const GROUPS: &[Group] = &[
    Group {
        name: "image",
        usage: "image <ls|inspect|tag|rm|save|prune|diff>",
        subs: &[
            ("ls", &images::IMAGES),
            ("list", &images::IMAGES),
            ("rm", &images::RMI),
            ("tag", &images::TAG),
            ("save", &images::SAVE),
            ("inspect", &images::INSPECT),
            ("prune", &images::PRUNE),
            ("diff", &images::DIFF),
        ],
    },
    Group {
        name: "server",
        usage: "server <ls|stop|rm|logs|restart>",
        subs: &[
            ("ls", &servers::PS),
            ("stop", &servers::STOP),
            ("rm", &servers::RM),
            ("logs", &servers::LOGS),
            ("restart", &servers::RESTART),
        ],
    },
    Group {
        name: "cache",
        usage: "cache <info|prune|du>",
        subs: &[
            ("info", &cache::INFO),
            ("prune", &cache::PRUNE),
            ("du", &cache::DU),
            ("ls", &cache::DU),
        ],
    },
];

fn is_extra_command(name: Option<&String>) -> bool {
    let Some(n) = name else { return false };
    TOP.iter().any(|c| c.name == n) || GROUPS.iter().any(|g| g.name == n)
}

fn run_extra_command(argv: &[String]) -> i32 {
    let name = &argv[0];
    let rest = &argv[1..];
    if let Some(g) = GROUPS.iter().find(|g| g.name == name) {
        let sub = rest.first();
        let cmd = sub
            .and_then(|s| g.subs.iter().find(|(n, _)| n == s))
            .map(|(_, c)| *c);
        let Some(cmd) = cmd else {
            if sub.is_none() || sub.is_some_and(|s| s == "--help" || s == "-h") {
                let mut seen: Vec<*const Command> = Vec::new();
                let mut lines = Vec::new();
                for (_, c) in g.subs {
                    let p = *c as *const Command;
                    if seen.contains(&p) {
                        continue;
                    }
                    seen.push(p);
                    lines.push(format!("  {:<44} {}", c.usage, c.summary));
                }
                out(&format!(
                    "Usage: sitefile {} ...\n\n{}\n",
                    g.usage,
                    lines.join("\n")
                ));
                return if sub.is_none() { 2 } else { 0 };
            }
            return usage_error(&format!(
                "unknown {name} command '{}'",
                sub.map(String::as_str).unwrap_or("")
            ));
        };
        return run_command(cmd, &rest[1..]);
    }
    match TOP.iter().find(|c| c.name == name) {
        Some(c) => run_command(c, rest),
        None => usage_error(&format!("unknown command '{name}'")),
    }
}

// ------------------------------------------------------------ build / validate / inspect

fn positive_int(raw: &str, flag: &str) -> Result<u64> {
    let n = js_to_number(raw);
    if !is_safe_integer(n) || n <= 0.0 {
        return Err(SitefileError::plain(
            ErrorCode::E_USAGE,
            format!("{flag} expects a positive integer"),
        ));
    }
    Ok(n as u64)
}

use crate::executor::{validate_image_tags, ImageInfo};

/// Store a build's exported tree as an OCI image in the local store and point each tag at it.
fn store_build_as_image(
    result: &BuildResult,
    tags: &[String],
    info: &ImageInfo,
    build_options: &BuildOptions,
) -> Result<String> {
    let store = build_options.open_store()?;
    let stored = crate::executor::store_build_as_image(&store, result, tags, info, build_options)?;
    let n = result.output.len();
    for key in &stored.tags {
        out(&format!(
            "Tagged {key} {} ({n} file{})\n",
            short_id(&stored.id),
            if n == 1 { "" } else { "s" }
        ));
    }
    Ok(stored.id)
}

fn run_main(argv: &[String]) -> i32 {
    if argv.first().map(String::as_str) == Some("lsp") {
        return crate::lsp::run(&argv[1..]);
    }
    if is_extra_command(argv.first()) {
        return run_extra_command(argv);
    }
    let o = match parse(argv, BUILD_OPTS) {
        Ok(p) => p,
        Err(m) => return usage_error(&m),
    };
    if o.flag("version") {
        out(&format!("{VERSION}\n"));
        return 0;
    }
    let pos = &o.positionals;
    let command = pos.first().cloned();
    if o.flag("help") || command.is_none() {
        out(&help());
        return if o.flag("help") { 0 } else { 2 };
    }
    let command = command.unwrap_or_default();
    // why <path> [context]
    let mut rest: Vec<String> = pos[1..].to_vec();
    let why_path = if command == "why" {
        if rest.is_empty() {
            return usage_error(
                "why expects a path in the build output: sitefile why <path> [context]",
            );
        }
        Some(rest.remove(0))
    } else {
        None
    };
    let context = rest.first().cloned().unwrap_or_else(|| ".".into());
    if rest.len() > 1 {
        return usage_error(&format!("unexpected arguments: {}", rest[1..].join(" ")));
    }
    let git_context = git_context::is_git_context(&context);
    if git_context && o.flag("watch") {
        return usage_error("--watch needs a local context directory, not a Git URL");
    }
    if let (true, Some(f)) = (git_context, o.str("file")) {
        if f.starts_with('/') || f.split(['\\', '/']).any(|s| s == "..") {
            return usage_error(
                "with a Git URL context, -f/--file is a path inside the repository",
            );
        }
    }
    if o.flag("dot") && command != "graph" {
        return usage_error("--dot is only for graph");
    }
    if o.flag("dot") && o.flag("json") {
        return usage_error("--dot and --json conflict");
    }
    if o.flag("frozen-lockfile") && o.flag("update-lock") {
        return usage_error("--frozen-lockfile and --update-lock conflict");
    }
    if o.flag("strict-pins") && o.flag("update-lock") {
        return usage_error("--strict-pins and --update-lock conflict");
    }
    let lint_disabled = lint_rules_off(&o);
    if o.has("lint-disable") && command != "validate" {
        return usage_error("--lint-disable is only for validate");
    }
    if let Some(id) = lint_disabled
        .iter()
        .find(|id| *id != "all" && !crate::lint::LINT_RULES.contains(&id.as_str()))
    {
        return usage_error(&format!(
            "--lint-disable: unknown lint rule '{id}' (rules: {}, or all)",
            crate::lint::LINT_RULES.join(", ")
        ));
    }
    let (cache_from, cache_to) = (o.strs("cache-from"), o.strs("cache-to"));
    if !cache_from.is_empty() || !cache_to.is_empty() {
        if command != "build" {
            return usage_error("--cache-from and --cache-to are only for build");
        }
        if o.flag("watch") {
            return usage_error("--watch cannot be combined with --cache-from or --cache-to");
        }
        if o.flag("no-cache") {
            return usage_error("--no-cache cannot be combined with --cache-from or --cache-to");
        }
        if cache_to.len() > 1 {
            return usage_error("--cache-to can be given only once");
        }
    }
    if o.flag("watch") {
        if command != "build" {
            return usage_error(&format!(
                "--watch is only for build (and serve --watch), not {command}"
            ));
        }
        if !o.strs("tag").is_empty() {
            return usage_error("--watch cannot be combined with -t");
        }
        servers::catch_signals();
        let arg_files = o.strs("build-arg-file");
        let opts = crate::watch::WatchOptions {
            context: &context,
            sitefile: o.str("file").filter(|s| !s.is_empty()),
            arg_files: &arg_files,
        };
        if o.str("profile") == Some("") {
            return usage_error("--profile expects a file path");
        }
        let memo = crate::memo::watch_memo();
        let mut build = || {
            let mut wrote = Vec::new();
            let code = run_profiled(&command, &context, None, &o, &mut wrote, memo.clone());
            crate::watch::BuildOutcome { code, wrote }
        };
        return crate::watch::watch_loop(&opts, &mut build, &mut || Ok(()), &|| {
            servers::received().is_some()
        })
        .unwrap_or_else(|e| report(&e));
    }
    if o.str("profile") == Some("") {
        return usage_error("--profile expects a file path");
    }
    run_profiled(
        &command,
        &context,
        why_path.as_deref(),
        &o,
        &mut Vec::new(),
        None,
    )
}

/// One build/validate/inspect/graph/why run with its cache eviction and, with `--profile`, its trace.
/// `memo`: results carried between the builds of a watch session (incremental rebuilds).
fn run_profiled(
    command: &str,
    context: &str,
    why_path: Option<&str>,
    o: &args::Parsed,
    wrote: &mut Vec<String>,
    memo: Option<crate::memo::SharedMemo>,
) -> i32 {
    let profile = o.str("profile").map(str::to_string);
    let profiler = profile.as_ref().map(|_| Profiler::new(command));
    if let Some(p) = &profile {
        wrote.push(p.clone());
    }
    let span = profiler
        .as_ref()
        .map(|p| p.begin(command, "command", Vec::new()));
    let mut failed = false;
    let code = match run_build_command(command, context, why_path, o, wrote, profiler.clone(), memo)
    {
        Ok(code) => code,
        Err(e) => {
            failed = true;
            report(&e)
        }
    };
    // Keep $SITEFILE_HOME under $SITEFILE_CACHE_MAX (never fails the command).
    if command == "build" || command == "inspect" || command == "why" {
        cache::evict_after_build();
    }
    if let Some(s) = span {
        s.end(Vec::new());
    }
    if let (Some(p), Some(path)) = (&profiler, &profile) {
        // A failed build already reported its error; otherwise this is the command's error.
        if std::fs::write(path, p.to_json()).is_err() && !failed {
            err(&format!(
                "error[E_EXPORT]: cannot write the --profile file '{path}'\n"
            ));
            return 1;
        }
    }
    code
}

/// `plan_build`, traced as `parse` with `--profile` (validate, graph).
fn profiled_plan(options: &BuildOptions) -> Result<crate::planner::Plan> {
    let span = options
        .profiler
        .as_ref()
        .map(|p| p.begin("parse", "build", Vec::new()));
    let p = plan_build(options)?;
    if let Some(s) = span {
        let mut args = vec![
            ("imports", json!(p.imports.len())),
            ("stages", json!(p.stages.len())),
        ];
        if let Some(t) = &p.target {
            args.push(("target", json!(t)));
        }
        s.end(args);
    }
    Ok(p)
}

/// `--cache-from` / `--cache-to`: the registry cache for this build, if any (E_USAGE for a bad reference).
fn registry_cache(o: &args::Parsed) -> Result<Option<Arc<crate::registry_cache::RegistryCache>>> {
    use crate::registry_cache::{parse_cache_ref, RegistryCache, RegistryCacheOptions};
    let (from, to) = (o.strs("cache-from"), o.strs("cache-to"));
    if from.is_empty() && to.is_empty() {
        return Ok(None);
    }
    let from = from
        .iter()
        .map(|r| parse_cache_ref("--cache-from", r))
        .collect::<Result<Vec<_>>>()?;
    let to = match to.first() {
        Some(r) => Some(parse_cache_ref("--cache-to", r)?),
        None => None,
    };
    let env = crate::registry::process_env();
    let reg_options = crate::registry::RegistryOptions {
        insecure_registries: crate::registry::insecure_registries_from_env(
            &o.strs("insecure-registry"),
            &env,
        ),
        allow_private_network: o.flag("allow-private-network"),
        ..Default::default()
    };
    let client: crate::registry_cache::ClientFactory = Arc::new(move |registry: &str| {
        crate::registry::make_client(registry, &reg_options, None, &env)
    });
    let warn: Callback = Arc::new(|m: &str| err(&format!("warning: {m}\n")));
    let notice: Callback = Arc::new(|m: &str| err(&format!("{m}\n")));
    let limit = crate::cache_limit::cache_limit_from_env(&mut |_| {});
    Ok(Some(Arc::new(RegistryCache::new(RegistryCacheOptions {
        from,
        to,
        client,
        limit,
        warn,
        notice,
    }))))
}

/// One build/validate/inspect/graph/why run (`why_path`: the path `why` explains); `wrote` receives the export
/// destination (for --watch).
#[allow(clippy::too_many_arguments)]
fn run_build_command(
    command: &str,
    context: &str,
    why_path: Option<&str>,
    o: &args::Parsed,
    wrote: &mut Vec<String>,
    profiler: Option<Arc<Profiler>>,
    memo: Option<crate::memo::SharedMemo>,
) -> Result<i32> {
    let mut fetch_policy = crate::resources::types::DEFAULT_FETCH_POLICY;
    fetch_policy.allow_http = o.flag("allow-http");
    fetch_policy.allow_private_network = o.flag("allow-private-network");
    if let Some(m) = o.str("max-download-bytes").filter(|s| !s.is_empty()) {
        fetch_policy.max_bytes = positive_int(m, "--max-download-bytes")?;
    }
    if let Some(t) = o.str("timeout").filter(|s| !s.is_empty()) {
        fetch_policy.timeout_ms = positive_int(t, "--timeout")?;
    }
    // A Git URL context: fetched into a temporary directory, built from there, then removed (on drop).
    let fetched = if git_context::is_git_context(context) {
        let g = git_context::materialize_git_context(
            context,
            &fetch_policy,
            &crate::resources::types::DEFAULT_ARCHIVE_LIMITS,
        )?;
        err(&format!("context: {context} at commit {}\n", g.commit));
        Some(g)
    } else {
        None
    };
    let context_owned = fetched
        .as_ref()
        .map(|g| g.dir.to_string_lossy().into_owned());
    let context: &str = context_owned.as_deref().unwrap_or(context);
    let remote_cache = registry_cache(o)?;
    // Precedence: Sitefile defaults < --build-arg-file (later files win) < --build-arg.
    let build_args = collect_build_args(&o.strs("build-arg-file"), &o.strs("build-arg"))?;
    let secrets = load_secrets(&o.strs("secret"), &process_env)?;
    let mut format_override: Option<ExportFormat> = None;
    if let Some(f) = o.str("format") {
        format_override = ExportFormat::parse(f);
        if format_override.is_none() {
            return Ok(usage_error(&format!(
                "--format expects one of {} (or tar.gz), got '{f}'",
                images::EXPORT_FORMATS
            )));
        }
    }
    let mut oci_compression = None;
    if let Some(c) = o.str("oci-compression") {
        oci_compression = OciCompression::parse(c);
        if oci_compression.is_none() {
            return Ok(usage_error(&format!(
                "--oci-compression expects gzip or none, got '{c}'"
            )));
        }
    }
    let mut compression = None;
    if let Some(c) = o.str("compression") {
        compression = OciCompression::parse(c);
        if compression.is_none() {
            return Ok(usage_error(&format!(
                "--compression expects gzip or none, got '{c}'"
            )));
        }
        if oci_compression.is_some() {
            return Ok(usage_error(
                "--compression and --oci-compression set the same thing; give one",
            ));
        }
    }
    // No --format: an archive extension on --out picks the format.
    let out_flag = o.str("out").filter(|s| !s.is_empty());
    if format_override.is_none() {
        if let Some(out) = out_flag {
            format_override = format_from_extension(out);
        }
    }
    let file = o.str("file").filter(|s| !s.is_empty()).map(|f| {
        if fetched.is_some() {
            join_path(context, f)
        } else {
            f.to_string()
        }
    });
    let mut options = BuildOptions {
        context_dir: context.to_string(),
        sitefile: file,
        build_args,
        secrets,
        target: o.str("target").map(str::to_string),
        ..Default::default()
    };
    if fetched.is_some() {
        options.label = Some(o.str("file").unwrap_or("Sitefile").to_string());
    }
    // The default destination depends on the format, which the Sitefile may set.
    // (A wbn's default name also depends on its COMPRESSION: dist.wbn or dist.wbn.gz.)
    let sitefile_plan = if command == "build"
        && out_flag.is_none()
        && format_override.is_none_or(|f| f == ExportFormat::Wbn)
    {
        Some(plan_build(&options)?)
    } else {
        None
    };
    let sitefile_format = match format_override {
        Some(_) => None,
        None => sitefile_plan.as_ref().map(|p| p.export_format),
    };
    let default_format = format_override
        .or(sitefile_format)
        .unwrap_or(ExportFormat::Directory);
    let default_compression = if default_format == ExportFormat::Wbn {
        compression.or_else(|| {
            sitefile_plan
                .as_ref()
                .and_then(|p| sitefile_compression(p, ExportFormat::Wbn))
        })
    } else {
        None
    };
    let default_name = default_out_name(default_format, default_compression);
    let out_dir = match out_flag {
        Some(o) => o.to_string(),
        // A Git context's default destination is in the current directory, not the temporary checkout.
        None if fetched.is_some() => default_name.to_string(),
        None => join_path(context, default_name),
    };
    options.out_dir = Some(out_dir.clone());
    wrote.push(out_dir.clone());
    options.fetch_policy = fetch_policy;
    if o.flag("no-lockfile") {
        options.lockfile = LockfileSetting::Disabled;
    }
    options.vendor_dir = o.str("vendor-dir").map(str::to_string);
    if let Some(t) = o.str("plugin-timeout").filter(|s| !s.is_empty()) {
        let ms = positive_int(t, "--plugin-timeout")?;
        options.plugin_policy.timeout_ms = ms;
        options.tool_policy.timeout_ms = ms;
    }
    options.allow_exec = o.strs("allow-exec");
    options.allow_plugin = o.strs("allow-plugin");
    options.allow_all_plugins = o.flag("allow-plugins");
    if !o.flag("no-cache") {
        options.cache_dir = Some(cache::transform_cache_dir());
    }
    if let Some(rc) = &remote_cache {
        let rc: Arc<dyn crate::plugins::cache::RemoteCache> = rc.clone();
        options.remote_cache = Some(rc);
    }
    let notice: Callback = Arc::new(|m: &str| err(&format!("{m}\n")));
    options.on_notice = Some(notice);
    options.allow_hardlinks = o.flag("allow-hardlinks");
    options.platform = o.str("platform").map(str::to_string);
    options.base_image = o.str("base").map(str::to_string);
    let tags = o.strs("tag");
    // inspect never pulls; a non-oci export (with no image tag) has no use for a BASE
    options.skip_base = command == "inspect"
        || command == "why"
        || (command == "build"
            && format_override.is_some_and(|f| f != ExportFormat::Oci)
            && tags.is_empty());
    options.insecure_registries =
        registry::insecure_registries_from_env(&o.strs("insecure-registry"));
    options.frozen_lockfile = o.flag("frozen-lockfile");
    options.update_lock = o.flag("update-lock");
    options.strict_pins = o.flag("strict-pins");
    let warning: Callback = Arc::new(|m: &str| err(&format!("warning: {m}\n")));
    options.on_warning = Some(warning);
    options.memo = memo;
    if let Some(p) = profiler {
        options.also_writes = o.str("profile").map(str::to_string).into_iter().collect();
        options.profiler = Some(p);
    }
    // --compression belongs to the export's own format, oci or wbn (-t without -o stores an oci image);
    // a *.wbn.gz destination means a gzipped bundle.
    let mut wbn_compression = None;
    if command == "build" {
        if let Some(c) = compression {
            let format = if !tags.is_empty() && out_flag.is_none() {
                ExportFormat::Oci
            } else {
                match format_override.or(sitefile_format) {
                    Some(f) => f,
                    None => plan_build(&options)?.export_format,
                }
            };
            match format {
                ExportFormat::Oci => oci_compression = Some(c),
                ExportFormat::Wbn => wbn_compression = Some(c),
                f => {
                    return Ok(usage_error(&format!(
                        "--compression applies only to oci and wbn exports, not {}",
                        f.as_str()
                    )))
                }
            }
        }
    }
    if format_override == Some(ExportFormat::Wbn) {
        if let Some(out) = out_flag {
            wbn_compression =
                wbn_compression.or(compression_from_extension(out, ExportFormat::Wbn));
        }
    }

    match command {
        "build" => {
            validate_image_tags(&tags)?;
            let labels = parse_labels(&o.strs("label"))?;
            let attest = AttestRequest {
                provenance: o.flag("provenance"),
                sbom: o.flag("sbom"),
            };
            if attest.any() && tags.is_empty() {
                let format = match format_override.or(sitefile_format) {
                    Some(f) => f,
                    None => plan_build(&options)?.export_format,
                };
                if format != ExportFormat::Oci {
                    return Ok(usage_error(
                        "--provenance and --sbom attach attestations to an oci image: export one (--format oci, -o *.oci.tar, EXPORT ... AS oci) or store one with -t",
                    ));
                }
            }
            let info = ImageInfo {
                context_dir: context.to_string(),
                oci_root: o.str("oci-root").map(str::to_string),
                oci_compression,
                platform: o.str("platform").map(str::to_string),
                labels: labels.clone(),
                git_labels: o.flag("git-labels"),
                attest,
            };
            if let Some(rc) = &remote_cache {
                rc.load();
            }
            let push_cache = || {
                if let Some(rc) = &remote_cache {
                    rc.push(&cache::transform_cache_dir());
                }
            };
            if !tags.is_empty() && out_flag.is_none() {
                // -t without -o: store the image only; nothing is written to the context.
                store_build_as_image(&build(&options)?, &tags, &info, &options)?;
                push_cache();
                return Ok(0);
            }
            let target = ExportTarget {
                format: format_override,
                base_url: o.str("base-url").map(str::to_string),
                oci_root: o.str("oci-root").map(str::to_string),
                oci_compression,
                wbn_compression,
                image_labels: labels.clone(),
                git_labels: o.flag("git-labels"),
                oci_ref_name: match tags.first() {
                    Some(t) => Some(ref_key(&parse_local_ref(t)?)),
                    None => None,
                },
                force: o.flag("force"),
                attest,
            };
            let r = build_to_directory(&options, &out_dir, &target)?;
            let cwd = std::env::current_dir().unwrap_or_default();
            let rel = relative_path(&cwd, &r.out_path);
            let where_ = if rel.is_empty() { ".".to_string() } else { rel };
            let size = r.result.output.len();
            let n = format!("{size} file{}", if size == 1 { "" } else { "s" });
            let line = match r.format {
                ExportFormat::Directory => {
                    format!("Exported {n} to {where_} ({})\n", r.result.output.digest())
                }
                ExportFormat::Oci => format!(
                    "Exported {n} to {where_} (oci image {}{})\n",
                    r.manifest_digest.as_deref().unwrap_or(""),
                    r.bytes.map(|b| format!(", {b} bytes")).unwrap_or_default()
                ),
                f => format!(
                    "Exported {n} to {where_} ({}, {} bytes, {})\n",
                    if f == ExportFormat::Tgz {
                        "tar.gz"
                    } else {
                        f.as_str()
                    },
                    r.bytes.unwrap_or(0),
                    r.file_digest.as_deref().unwrap_or("")
                ),
            };
            out(&line);
            // A Git context's lockfile lives in the temporary checkout: it is used, never written back.
            if r.result.lockfile_written && fetched.is_none() {
                out("Updated Sitefile.lock\n");
            }
            if !tags.is_empty() {
                store_build_as_image(&r.result, &tags, &info, &options)?;
            }
            push_cache();
            Ok(0)
        }
        "validate" => {
            let p = profiled_plan(&options)?;
            for m in &p.warnings {
                err(&format!("warning: {m}\n"));
            }
            for m in crate::executor::lint_build(&options, &lint_rules_off(o))? {
                err(&format!("warning: {m}\n"));
            }
            if options.strict_pins {
                check_plan_pins(&options, &p)?;
            }
            if o.flag("json") {
                out(&(pretty_json(&p.validate_json()) + "\n"));
            } else {
                let ops: usize = p.stages.iter().map(|s| s.operations.len()).sum();
                let n = p.imports.len();
                out(&format!(
                    "{}: OK ({n} import{}, {ops} operation{}{}{})\n",
                    p.file,
                    if n == 1 { "" } else { "s" },
                    if ops == 1 { "" } else { "s" },
                    if p.stages.len() > 1 {
                        format!(", {} stages", p.stages.len())
                    } else {
                        String::new()
                    },
                    p.target
                        .as_ref()
                        .map(|t| format!(", target {t}"))
                        .unwrap_or_default()
                ));
            }
            Ok(0)
        }
        "inspect" => {
            options.frozen_lockfile = true;
            let r = build(&options)?;
            let files: Vec<_> = r.output.entries().collect();
            if o.flag("json") {
                let list: Vec<_> = files
                    .iter()
                    .map(|(p, n)| json!({ "path": p, "mode": format!("{:o}", n.mode), "size": n.bytes.len(), "digest": n.digest, "origin": n.origin }))
                    .collect();
                out(&(pretty_json(&json!({ "digest": r.output.digest(), "files": list })) + "\n"));
            } else {
                let mut s = String::new();
                for (p, n) in &files {
                    s.push_str(&format!(
                        "{:o}  {:>8}  {p}  ({})\n",
                        n.mode,
                        n.bytes.len(),
                        n.origin
                    ));
                }
                s.push_str(&format!("{} files, {}\n", files.len(), r.output.digest()));
                out(&s);
            }
            Ok(0)
        }
        "graph" => {
            let p = profiled_plan(&options)?;
            for m in &p.warnings {
                err(&format!("warning: {m}\n"));
            }
            if options.strict_pins {
                check_plan_pins(&options, &p)?;
            }
            let g = crate::explain::plan_graph(&p);
            out(&if o.flag("json") {
                pretty_json(&crate::explain::graph_json(&g)) + "\n"
            } else if o.flag("dot") {
                crate::explain::graph_dot(&g)
            } else {
                crate::explain::graph_text(&g)
            });
            Ok(0)
        }
        "why" => {
            options.frozen_lockfile = true;
            options.record_steps = true;
            let r = build(&options)?;
            let report = crate::explain::why(&r, why_path.unwrap_or_default(), context)?;
            out(&if o.flag("json") {
                pretty_json(&crate::explain::why_json(&report)) + "\n"
            } else {
                crate::explain::why_text(&report)
            });
            Ok(0)
        }
        _ => Ok(usage_error(&format!("unknown command '{command}'"))),
    }
}

/// Entry point; returns the process exit code (0 ok, 1 build error, 2 usage, 70 internal error).
pub fn main(args: Vec<String>) -> i32 {
    if args.first().map(String::as_str) == Some("__serve-worker") {
        return servers::serve_worker(args.get(1).map(String::as_str).unwrap_or(""));
    }
    std::panic::set_hook(Box::new(|info| {
        err(&format!("internal error: {info}\n"));
    }));
    match std::panic::catch_unwind(|| run_main(&args)) {
        Ok(code) => code,
        Err(_) => 70,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_mentions_defaults() {
        let h = help();
        assert!(h.starts_with("sitefile 0.1.0 — Docker-inspired static artifact builder\n"));
        assert!(h.contains("per-resource size limit (default 52428800)"));
        assert!(h.ends_with("Run 'sitefile <command> --help' for the options of a command.\n"));
    }

    #[test]
    fn commands_are_known() {
        assert!(is_extra_command(Some(&"images".to_string())));
        assert!(is_extra_command(Some(&"cache".to_string())));
        assert!(!is_extra_command(Some(&"build".to_string())));
    }
}

/// `--lint-disable` values: comma-separated rule ids (repeatable).
fn lint_rules_off(o: &args::Parsed) -> Vec<String> {
    o.strs("lint-disable")
        .iter()
        .flat_map(|v| {
            v.split(',')
                .map(|s| crate::language::scanner::js_trim(s).to_string())
                .collect::<Vec<_>>()
        })
        .collect()
}

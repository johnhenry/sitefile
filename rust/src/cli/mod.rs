//! The `sitefile` command line (port of `src/cli.ts` and `src/cli/*.ts`):
//! the same commands, flags, help text, output lines and exit codes
//! (0 ok, 1 build error, 2 usage, 70 internal error).

pub mod args;
pub mod cache;
pub mod common;
pub mod images;
pub mod registry;
pub mod servers;

use crate::build_args::{load_build_arg_file, BuildArgs};
use crate::error::{ErrorCode, Result, SitefileError};
use crate::executor::{build, build_to_directory, join_path, oci_options, plan_build, relative_path, BuildOptions, BuildResult, Callback, ExportTarget, LockfileSetting};
use crate::export::formats::{default_out_name, format_from_extension};
use crate::export::oci::parse_labels;
use crate::language::ast::{ExportFormat, OciCompression};
use crate::refs::{parse_local_ref, ref_key};
use crate::resources::types::FetchPolicy;
use crate::store::{short_id, Store};
use args::{flag, flag_s, parse, string, string_s, strings, strings_s, OptSpec};
use common::{err, is_safe_integer, js_to_number, out, pretty_json, report, run_command, usage_error, Command};
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;

pub const VERSION: &str = "0.1.0";

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

Build cache (TRANSFORM results: $SITEFILE_HOME/cache/transform):
  sitefile cache info [--json]                     location, entries, size
  sitefile cache prune [--older-than <days>]       delete cached results

Serving sites (detached servers are tracked by name):
  sitefile serve [--name n] [-p [host:]port] [-d] [--spa] <ref|./dir>
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

Options:
  -f, --file <path>            Sitefile path (default: <context>/Sitefile)
  -o, --out <path>             export destination (build only): a directory for 'directory',
                               a file for zip, tar, tgz, wbn
  -t, --tag <name[:tag]>       also store the build as a local image (build only, repeatable);
                               with -t and no -o nothing is written to the context
      --format <fmt>           directory | zip | tar | tgz (alias tar.gz) | wbn | oci (build only).
                               oci writes an OCI archive for -o *.tar (e.g. site.oci.tar), else an
                               OCI image layout directory.
                               Default: the --out extension (.zip .tar .tar.gz .tgz .wbn .oci.tar),
                               else the Sitefile's EXPORT ... AS <fmt>
      --oci-root <path>        directory inside an oci image layer (default /srv/www; overrides EXPORT ... AT)
      --oci-compression <c>    oci site layer: gzip (default) or none (plain tar; overrides EXPORT ... COMPRESSION)
      --label <key=value>      add an OCI image config label (repeatable)
      --git-labels             add org.opencontainers.image.source/revision from the git checkout
                               (off by default: they change the image ID on every commit)
      --platform <os/arch>     oci image platform (default linux/amd64); with a BASE, the platform pulled
      --base <image>           build the oci image on <image> (runnable; overrides EXPORT ... AS oci BASE);
                               a bare name means Docker Hub. Pin it: nginx:alpine@sha256:...
      --insecure-registry <h>  allow plain http to registry host[:port] (repeatable; localhost always may)
      --base-url <url>         https URL ending in '/' the files are mounted at (wbn only);
                               overrides EXPORT ... BASE
      --build-arg <NAME=value> set an ARG (repeatable); --build-arg NAME reads $NAME
      --plugin-timeout <ms>    per-call time limit for TRANSFORM plugins and tools (default 60000)
      --allow-exec <name[=path]> allow a TOOL (external program) to run, resolved on PATH or at
                               the absolute path (repeatable); programs run with your privileges
      --no-cache               do not read or write the TRANSFORM result cache
                               ($SITEFILE_HOME/cache/transform; see 'sitefile cache --help')
      --build-arg-file <path>  read ARGs from a .json, .yaml/.yml or .env file (repeatable;
                               later files win, --build-arg wins over files)
      --force                  replace a non-empty --out outside the context
      --frozen-lockfile        fail instead of changing Sitefile.lock
      --update-lock            re-record unpinned URLs whose content changed
      --no-lockfile            do not read or write Sitefile.lock
      --allow-http             permit plain http:// imports
      --allow-private-network  permit loopback/private network addresses
      --allow-hardlinks        permit hard-linked files in the context (you assert they are safe)
      --max-download-bytes <n> per-resource size limit (default {})
      --timeout <ms>           per-resource fetch timeout (default {})
      --json                   machine-readable output (validate, inspect)
  -h, --help                   show this help
  -v, --version                show the version
{EXTRA_HELP}",
        d.max_bytes, d.timeout_ms
    )
}

const BUILD_OPTS: &[OptSpec] = &[
    string_s("file", 'f'),
    string_s("out", 'o'),
    strings_s("tag", 't'),
    string("format"),
    string("base-url"),
    string("oci-root"),
    string("oci-compression"),
    strings("label"),
    flag("git-labels"),
    string("platform"),
    string("base"),
    strings("insecure-registry"),
    strings("build-arg"),
    strings("build-arg-file"),
    string("plugin-timeout"),
    strings("allow-exec"),
    flag("no-cache"),
    flag("force"),
    flag("frozen-lockfile"),
    flag("update-lock"),
    flag("no-lockfile"),
    flag("allow-http"),
    flag("allow-private-network"),
    flag("allow-hardlinks"),
    string("max-download-bytes"),
    string("timeout"),
    flag("json"),
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
        subs: &[("ls", &servers::PS), ("stop", &servers::STOP), ("rm", &servers::RM), ("logs", &servers::LOGS), ("restart", &servers::RESTART)],
    },
    Group { name: "cache", usage: "cache <info|prune>", subs: &[("info", &cache::INFO), ("prune", &cache::PRUNE)] },
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
        let cmd = sub.and_then(|s| g.subs.iter().find(|(n, _)| n == s)).map(|(_, c)| *c);
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
                out(&format!("Usage: sitefile {} ...\n\n{}\n", g.usage, lines.join("\n")));
                return if sub.is_none() { 2 } else { 0 };
            }
            return usage_error(&format!("unknown {name} command '{}'", sub.map(String::as_str).unwrap_or("")));
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
        return Err(SitefileError::plain(ErrorCode::E_USAGE, format!("{flag} expects a positive integer")));
    }
    Ok(n as u64)
}

fn is_identifier(s: &str) -> bool {
    let mut c = s.chars();
    c.next().is_some_and(|f| f.is_ascii_alphabetic() || f == '_') && c.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// Validate `-t` values before building: local name[:tag] only.
fn validate_image_tags(tags: &[String]) -> Result<()> {
    for t in tags {
        if parse_local_ref(t)?.digest.is_some() {
            return Err(SitefileError::plain(ErrorCode::E_USAGE, format!("-t '{t}': a tag cannot include a digest")));
        }
    }
    Ok(())
}

struct ImageInfo<'a> {
    context_dir: &'a str,
    oci_root: Option<&'a str>,
    oci_compression: Option<OciCompression>,
    platform: Option<&'a str>,
    labels: &'a BTreeMap<String, String>,
    git_labels: bool,
}

/// Store a build's exported tree as an OCI image in the local store and point each tag at it.
fn store_build_as_image(result: &BuildResult, tags: &[String], info: &ImageInfo<'_>) -> Result<String> {
    let store = Store::open_default()?;
    let opts = oci_options(result, info.context_dir, info.oci_root, info.oci_compression, info.platform, info.labels, info.git_labels)?;
    let added = store.add_image(&result.output, &opts)?;
    let n = result.output.len();
    for t in tags {
        let key = store.tag(&added.id, &parse_local_ref(t)?)?;
        out(&format!("Tagged {key} {} ({n} file{})\n", short_id(&added.id), if n == 1 { "" } else { "s" }));
    }
    Ok(added.id)
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
    let context = pos.get(1).cloned().unwrap_or_else(|| ".".into());
    if pos.len() > 2 {
        return usage_error(&format!("unexpected arguments: {}", pos[2..].join(" ")));
    }
    if o.flag("frozen-lockfile") && o.flag("update-lock") {
        return usage_error("--frozen-lockfile and --update-lock conflict");
    }
    match run_build_command(&command, &context, &o) {
        Ok(code) => code,
        Err(e) => report(&e),
    }
}

fn run_build_command(command: &str, context: &str, o: &args::Parsed) -> Result<i32> {
    // Precedence: Sitefile defaults < --build-arg-file (later files win) < --build-arg.
    let mut build_args = BuildArgs::new();
    for file in o.strs("build-arg-file") {
        build_args.extend_from(&load_build_arg_file(&file)?);
    }
    for kv in o.strs("build-arg") {
        match kv.find('=') {
            None => {
                // --build-arg NAME: take the value from the environment, as Docker does.
                if !is_identifier(&kv) {
                    return Ok(usage_error(&format!("--build-arg expects NAME=value or NAME, got '{kv}'")));
                }
                match std::env::var(&kv) {
                    Ok(v) => build_args.set(kv.clone(), v),
                    Err(_) => return Ok(usage_error(&format!("--build-arg {kv}: environment variable {kv} is not set"))),
                }
            }
            Some(0) => return Ok(usage_error(&format!("--build-arg expects NAME=value, got '{kv}'"))),
            Some(eq) => build_args.set(kv[..eq].to_string(), kv[eq + 1..].to_string()),
        }
    }
    let mut format_override: Option<ExportFormat> = None;
    if let Some(f) = o.str("format") {
        format_override = ExportFormat::parse(f);
        if format_override.is_none() {
            return Ok(usage_error(&format!("--format expects one of {} (or tar.gz), got '{f}'", images::EXPORT_FORMATS)));
        }
    }
    let mut oci_compression = None;
    if let Some(c) = o.str("oci-compression") {
        oci_compression = OciCompression::parse(c);
        if oci_compression.is_none() {
            return Ok(usage_error(&format!("--oci-compression expects gzip or none, got '{c}'")));
        }
    }
    // No --format: an archive extension on --out picks the format.
    let out_flag = o.str("out").filter(|s| !s.is_empty());
    if format_override.is_none() {
        if let Some(out) = out_flag {
            format_override = format_from_extension(out);
        }
    }
    let file = o.str("file").filter(|s| !s.is_empty()).map(str::to_string);
    let mut options = BuildOptions { context_dir: context.to_string(), sitefile: file, build_args, ..Default::default() };
    // The default destination depends on the format, which the Sitefile may set.
    let sitefile_format = if command == "build" && out_flag.is_none() && format_override.is_none() { Some(plan_build(&options)?.export_format) } else { None };
    let out_dir = match out_flag {
        Some(o) => o.to_string(),
        None => join_path(context, default_out_name(format_override.or(sitefile_format).unwrap_or(ExportFormat::Directory))),
    };
    options.out_dir = Some(out_dir.clone());
    options.fetch_policy.allow_http = o.flag("allow-http");
    options.fetch_policy.allow_private_network = o.flag("allow-private-network");
    if let Some(m) = o.str("max-download-bytes").filter(|s| !s.is_empty()) {
        options.fetch_policy.max_bytes = positive_int(m, "--max-download-bytes")?;
    }
    if let Some(t) = o.str("timeout").filter(|s| !s.is_empty()) {
        options.fetch_policy.timeout_ms = positive_int(t, "--timeout")?;
    }
    if o.flag("no-lockfile") {
        options.lockfile = LockfileSetting::Disabled;
    }
    if let Some(t) = o.str("plugin-timeout").filter(|s| !s.is_empty()) {
        let ms = positive_int(t, "--plugin-timeout")?;
        options.plugin_policy.timeout_ms = ms;
        options.tool_policy.timeout_ms = ms;
    }
    options.allow_exec = o.strs("allow-exec");
    if !o.flag("no-cache") {
        options.cache_dir = Some(cache::transform_cache_dir());
    }
    let notice: Callback = Arc::new(|m: &str| err(&format!("{m}\n")));
    options.on_notice = Some(notice);
    options.allow_hardlinks = o.flag("allow-hardlinks");
    options.platform = o.str("platform").map(str::to_string);
    options.base_image = o.str("base").map(str::to_string);
    let tags = o.strs("tag");
    // inspect never pulls; a non-oci export (with no image tag) has no use for a BASE
    options.skip_base = command == "inspect" || (command == "build" && format_override.is_some_and(|f| f != ExportFormat::Oci) && tags.is_empty());
    options.insecure_registries = registry::insecure_registries_from_env(&o.strs("insecure-registry"));
    options.frozen_lockfile = o.flag("frozen-lockfile");
    options.update_lock = o.flag("update-lock");
    let warning: Callback = Arc::new(|m: &str| err(&format!("warning: {m}\n")));
    options.on_warning = Some(warning);

    match command {
        "build" => {
            validate_image_tags(&tags)?;
            let labels = parse_labels(&o.strs("label"))?;
            let info = ImageInfo { context_dir: context, oci_root: o.str("oci-root"), oci_compression, platform: o.str("platform"), labels: &labels, git_labels: o.flag("git-labels") };
            if !tags.is_empty() && out_flag.is_none() {
                // -t without -o: store the image only; nothing is written to the context.
                store_build_as_image(&build(&options)?, &tags, &info)?;
                return Ok(0);
            }
            let target = ExportTarget {
                format: format_override,
                base_url: o.str("base-url").map(str::to_string),
                oci_root: o.str("oci-root").map(str::to_string),
                oci_compression,
                image_labels: labels.clone(),
                git_labels: o.flag("git-labels"),
                oci_ref_name: match tags.first() {
                    Some(t) => Some(ref_key(&parse_local_ref(t)?)),
                    None => None,
                },
                force: o.flag("force"),
            };
            let r = build_to_directory(&options, &out_dir, &target)?;
            let cwd = std::env::current_dir().unwrap_or_default();
            let rel = relative_path(&cwd, &r.out_path);
            let where_ = if rel.is_empty() { ".".to_string() } else { rel };
            let size = r.result.output.len();
            let n = format!("{size} file{}", if size == 1 { "" } else { "s" });
            let line = match r.format {
                ExportFormat::Directory => format!("Exported {n} to {where_} ({})\n", r.result.output.digest()),
                ExportFormat::Oci => format!(
                    "Exported {n} to {where_} (oci image {}{})\n",
                    r.manifest_digest.as_deref().unwrap_or(""),
                    r.bytes.map(|b| format!(", {b} bytes")).unwrap_or_default()
                ),
                f => format!(
                    "Exported {n} to {where_} ({}, {} bytes, {})\n",
                    if f == ExportFormat::Tgz { "tar.gz" } else { f.as_str() },
                    r.bytes.unwrap_or(0),
                    r.file_digest.as_deref().unwrap_or("")
                ),
            };
            out(&line);
            if r.result.lockfile_written {
                out("Updated Sitefile.lock\n");
            }
            if !tags.is_empty() {
                store_build_as_image(&r.result, &tags, &info)?;
            }
            Ok(0)
        }
        "validate" => {
            let p = plan_build(&options)?;
            for m in &p.warnings {
                err(&format!("warning: {m}\n"));
            }
            if o.flag("json") {
                out(&(pretty_json(&p.validate_json()) + "\n"));
            } else {
                let ops: usize = p.stages.iter().map(|s| s.operations.len()).sum();
                let n = p.imports.len();
                out(&format!(
                    "{}: OK ({n} import{}, {ops} operation{}{})\n",
                    p.file,
                    if n == 1 { "" } else { "s" },
                    if ops == 1 { "" } else { "s" },
                    if p.stages.len() > 1 { format!(", {} stages", p.stages.len()) } else { String::new() }
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
                    s.push_str(&format!("{:o}  {:>8}  {p}  ({})\n", n.mode, n.bytes.len(), n.origin));
                }
                s.push_str(&format!("{} files, {}\n", files.len(), r.output.digest()));
                out(&s);
            }
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

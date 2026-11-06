//! `sitefile vendor` (port of `src/cli/vendor.ts`).

use super::args::{flag, string, string_s, strings, OptSpec, Parsed};
use super::common::{err, is_safe_integer, js_to_number, out, require_args, usage, Command};
use crate::build_args::collect_build_args;
use crate::error::Result;
use crate::executor::{relative_path, BuildOptions, Callback};
use crate::vendor::vendor;
use std::sync::Arc;

const FLAGS: &[&str] = &[
    "-f, --file <path>             Sitefile path (default: <context>/Sitefile)",
    "--vendor-dir <dir>            vendor directory, relative to the context (default vendor/sitefile)",
    "--build-arg <NAME=value>      set an ARG (repeatable); --build-arg NAME reads $NAME",
    "--build-arg-file <path>       read ARGs from a .json, .yaml/.yml or .env file (repeatable)",
    "--frozen-lockfile             fail instead of changing Sitefile.lock",
    "--update-lock                 re-record unpinned URLs whose content changed, re-resolve BASE tags",
    "--allow-http                  permit plain http:// imports",
    "--allow-private-network       permit loopback/private network addresses",
    "--max-download-bytes <n>      per-resource size limit (default 52428800)",
    "--timeout <ms>                per-resource fetch timeout (default 30000)",
    "--base <image>                vendor this BASE image (overrides EXPORT ... AS oci BASE)",
    "--platform <os/arch>          BASE platform to vendor (default linux/amd64)",
    "--insecure-registry <h>       allow plain http to registry host[:port] (repeatable)",
];

const OPTS: &[OptSpec] = &[
    string_s("file", 'f'),
    string("vendor-dir"),
    strings("build-arg"),
    strings("build-arg-file"),
    flag("frozen-lockfile"),
    flag("update-lock"),
    flag("allow-http"),
    flag("allow-private-network"),
    string("max-download-bytes"),
    string("timeout"),
    string("base"),
    string("platform"),
    strings("insecure-registry"),
];

pub const VENDOR: Command = Command {
    name: "vendor",
    summary: "download remote imports and the BASE image for offline builds",
    usage: "vendor [context] [options]",
    description: "Fetch every remote IMPORT and the BASE image (EXPORT ... AS oci BASE, or --base) as a build would, verify\nthem against INTEGRITY and Sitefile.lock (which is updated like a build updates it), and write them to\nthe vendor directory: content-addressed blobs (blobs/sha256/<hex>) and manifest.json. Later builds\nread vendored copies instead of the network (verified again) and never copy the directory into the\noutput. The directory is replaced as a whole; one that is not a vendor directory is refused.",
    flags: FLAGS,
    options: OPTS,
    run: run_vendor,
};

fn positive(raw: Option<&str>, flag: &str) -> Result<Option<u64>> {
    let Some(raw) = raw else { return Ok(None) };
    let n = js_to_number(raw);
    if !is_safe_integer(n) || n <= 0.0 {
        return usage(format!("{flag} expects a positive integer"));
    }
    Ok(Some(n as u64))
}

fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

fn run_vendor(v: &Parsed) -> Result<i32> {
    require_args(&v.positionals, 0, 1, "a context directory")?;
    if v.flag("frozen-lockfile") && v.flag("update-lock") {
        return usage("--frozen-lockfile and --update-lock conflict");
    }
    let context = v.positionals.first().cloned().unwrap_or_else(|| ".".into());
    let max_bytes = positive(v.str("max-download-bytes"), "--max-download-bytes")?;
    let timeout = positive(v.str("timeout"), "--timeout")?;
    let mut o = BuildOptions {
        context_dir: context,
        sitefile: v.str("file").filter(|s| !s.is_empty()).map(str::to_string),
        build_args: collect_build_args(&v.strs("build-arg-file"), &v.strs("build-arg"))?,
        vendor_dir: v.str("vendor-dir").map(str::to_string),
        base_image: v.str("base").map(str::to_string),
        platform: v.str("platform").map(str::to_string),
        insecure_registries: super::registry::insecure_registries_from_env(&v.strs("insecure-registry")),
        frozen_lockfile: v.flag("frozen-lockfile"),
        update_lock: v.flag("update-lock"),
        ..Default::default()
    };
    o.fetch_policy.allow_http = v.flag("allow-http");
    o.fetch_policy.allow_private_network = v.flag("allow-private-network");
    if let Some(m) = max_bytes {
        o.fetch_policy.max_bytes = m;
    }
    if let Some(t) = timeout {
        o.fetch_policy.timeout_ms = t;
    }
    let warning: Callback = Arc::new(|m: &str| err(&format!("warning: {m}\n")));
    o.on_warning = Some(warning);
    let r = vendor(&o)?;
    let cwd = std::env::current_dir().unwrap_or_default();
    let rel = relative_path(&cwd, &r.dir);
    let shown = if rel.is_empty() { ".".to_string() } else { rel };
    out(&format!(
        "Vendored {} and {} ({}, {} bytes) to {shown}\n",
        plural(r.resources, "resource"),
        plural(r.images, "image"),
        plural(r.blobs, "blob"),
        r.bytes
    ));
    if r.lockfile_written {
        out("Updated Sitefile.lock\n");
    }
    Ok(0)
}

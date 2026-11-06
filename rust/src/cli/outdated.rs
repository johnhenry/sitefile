//! `sitefile outdated` (port of `src/cli/outdated.ts`).

use super::args::{flag, string, string_s, strings, OptSpec, Parsed};
use super::common::{err, is_safe_integer, js_to_number, out, pretty_json, require_args, table, usage, Command};
use crate::build_args::collect_build_args;
use crate::error::Result;
use crate::executor::{BuildOptions, Callback, LockfileSetting};
use crate::outdated::{outdated, Report, Status};
use crate::store::short_id;
use std::sync::Arc;

const FLAGS: &[&str] = &[
    "-f, --file <path>             Sitefile path (default: <context>/Sitefile)",
    "--json                        machine-readable output",
    "--build-arg <NAME=value>      set an ARG (repeatable); --build-arg NAME reads $NAME",
    "--build-arg-file <path>       read ARGs from a .json, .yaml/.yml or .env file (repeatable)",
    "--no-lockfile                 ignore Sitefile.lock (only INTEGRITY and @sha256: count as pins)",
    "--allow-http                  permit plain http:// imports",
    "--allow-private-network       permit loopback/private network addresses",
    "--max-download-bytes <n>      per-resource size limit (default 52428800)",
    "--timeout <ms>                per-resource fetch timeout (default 30000)",
    "--base <image>                check this BASE image (overrides EXPORT ... AS oci BASE)",
    "--insecure-registry <h>       allow plain http to registry host[:port] (repeatable)",
];

const OPTS: &[OptSpec] = &[
    string_s("file", 'f'),
    flag("json"),
    strings("build-arg"),
    strings("build-arg-file"),
    flag("no-lockfile"),
    flag("allow-http"),
    flag("allow-private-network"),
    string("max-download-bytes"),
    string("timeout"),
    string("base"),
    strings("insecure-registry"),
];

pub const OUTDATED: Command = Command {
    name: "outdated",
    summary: "check whether pinned remote imports and BASE images have changed upstream",
    usage: "outdated [context] [options]",
    description: "Report, for every remote IMPORT and the BASE image, what it is pinned to (INTEGRITY, @sha256: or\nSitefile.lock) and what it resolves to now. Never changes anything: use 'sitefile build --update-lock'\nto accept new content. An https URL is fetched again and its sha256 compared (there is no notion of a\nnewer version of a URL); a BASE tag is resolved again (changed = the tag moved), and for a version-like\ntag (1.2, v1.2.3-alpine) the registry's tag list is searched for a greater tag of the same shape.",
    flags: FLAGS,
    options: OPTS,
    run: run_outdated,
};

fn positive(raw: Option<&str>, flag: &str) -> Result<Option<u64>> {
    let Some(raw) = raw else { return Ok(None) };
    let n = js_to_number(raw);
    if !is_safe_integer(n) || n <= 0.0 {
        return usage(format!("{flag} expects a positive integer"));
    }
    Ok(Some(n as u64))
}

/// The order statuses are counted in the summary line.
const ORDER: [Status; 5] = [Status::Changed, Status::Unpinned, Status::Error, Status::DigestOnly, Status::Current];

/// The text report: a table, one line per error, and a summary line.
pub fn format_outdated(r: &Report) -> String {
    if r.inputs.is_empty() {
        return "No remote inputs: the Sitefile has no remote IMPORT, no registry FROM image and no BASE image\n".into();
    }
    let short = |d: &Option<String>| d.as_deref().map(short_id).unwrap_or_else(|| "-".into());
    let mut rows: Vec<Vec<String>> = vec![["KIND", "NAME", "SOURCE", "PINNED", "BY", "LATEST", "STATUS", "NEWER TAG"].iter().map(|s| s.to_string()).collect()];
    for i in &r.inputs {
        rows.push(vec![
            i.kind.to_string(),
            i.name.clone(),
            i.source.clone(),
            short(&i.pin),
            i.pinned_by.unwrap_or("-").to_string(),
            short(&i.latest),
            i.status.as_str().to_string(),
            i.newer_tag.clone().unwrap_or_else(|| "-".into()),
        ]);
    }
    let mut s = table(&rows);
    for i in &r.inputs {
        if let Some((code, msg)) = &i.error {
            s.push_str(&format!("{}: error[{code}]: {msg}\n", i.name));
        }
    }
    let n = r.inputs.len();
    let counts: Vec<String> = ORDER
        .iter()
        .filter(|st| r.count(**st) > 0)
        .map(|st| {
            let k = r.count(*st);
            format!("{k} {}", if *st == Status::Error && k != 1 { "errors" } else { st.as_str() })
        })
        .collect();
    s.push_str(&format!("{n} remote input{}: {}\n", if n == 1 { "" } else { "s" }, counts.join(", ")));
    s
}

fn run_outdated(v: &Parsed) -> Result<i32> {
    require_args(&v.positionals, 0, 1, "a context directory")?;
    let max_bytes = positive(v.str("max-download-bytes"), "--max-download-bytes")?;
    let timeout = positive(v.str("timeout"), "--timeout")?;
    let mut o = BuildOptions {
        context_dir: v.positionals.first().cloned().unwrap_or_else(|| ".".into()),
        sitefile: v.str("file").filter(|s| !s.is_empty()).map(str::to_string),
        build_args: collect_build_args(&v.strs("build-arg-file"), &v.strs("build-arg"))?,
        base_image: v.str("base").map(str::to_string),
        insecure_registries: super::registry::insecure_registries_from_env(&v.strs("insecure-registry")),
        ..Default::default()
    };
    if v.flag("no-lockfile") {
        o.lockfile = LockfileSetting::Disabled;
    }
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
    let r = outdated(&o)?;
    if v.flag("json") {
        out(&(pretty_json(&r.to_json()) + "\n"));
    } else {
        out(&format_outdated(&r));
    }
    Ok(0)
}

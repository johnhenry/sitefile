//! `sitefile hosts` (port of `src/cli/hosts.ts`).

use super::args::{flag, string, string_s, strings, OptSpec, Parsed};
use super::common::{err, out, pretty_json, require_args, table, Command};
use crate::build_args::collect_build_args;
use crate::error::Result;
use crate::executor::{BuildOptions, Callback};
use crate::hosts::{hosts, HostStatus, HostUse, HostsReport};
use std::sync::Arc;

const FLAGS: &[&str] = &[
    "-f, --file <path>             Sitefile path (default: <context>/Sitefile)",
    "--json                        machine-readable output",
    "--build-arg <NAME=value>      set an ARG (repeatable); --build-arg NAME reads $NAME",
    "--build-arg-file <path>       read ARGs from a .json, .yaml/.yml or .env file (repeatable)",
    "--base <image>                include this BASE image (overrides EXPORT ... AS oci BASE)",
    "--proxy <url>                 send requests through this HTTP proxy: http://[user:password@]host[:port]",
    "                              (also $SITEFILE_PROXY; HTTP(S)_PROXY are never read)",
    "--no-proxy <hosts>            hosts reached directly, not through the proxy (NO_PROXY syntax; repeatable,",
    "                              comma-separated; also $SITEFILE_NO_PROXY)",
    "--allow-hosts <hosts>         contact only hosts matching these patterns (repeatable, comma-separated;",
    "                              also $SITEFILE_ALLOW_HOSTS)",
    "--deny-hosts <hosts>          never contact hosts matching these patterns; beats --allow-hosts",
    "                              (repeatable, comma-separated; also $SITEFILE_DENY_HOSTS)",
    "--network <mode>              open (default) or strict: every host must match --allow-hosts, even with none",
    "                              given (also $SITEFILE_NETWORK)",
    "--allow-secret-host <id=host> in strict mode, let IMPORT ... SECRET <id> send its secret to this host",
    "                              (a host pattern; repeatable)",
    "--connect-timeout <ms>        time limit for opening each connection (also $SITEFILE_FETCH_TIMEOUT)",
    "--read-timeout <ms>           longest wait for data on a connection (also $SITEFILE_FETCH_TIMEOUT)",
];

const OPTS: &[OptSpec] = &[
    string_s("file", 'f'),
    flag("json"),
    strings("build-arg"),
    strings("build-arg-file"),
    string("base"),
    string("proxy"),
    strings("no-proxy"),
    strings("allow-hosts"),
    strings("deny-hosts"),
    string("network"),
    strings("allow-secret-host"),
    string("connect-timeout"),
    string("read-timeout"),
];

pub const HOSTS: Command = Command {
    name: "hosts",
    summary: "list the hosts a build would contact, before contacting any",
    usage: "hosts [context] [options]",
    description: "List the hosts a build of the Sitefile would contact (remote IMPORTs, FROM image registries, the BASE\nimage's registry), what needs each, and whether the network rules (--allow-hosts, --deny-hosts,\n--network strict, --allow-secret-host) allow it. Nothing is fetched. Redirects, registry token and blob\nhosts and the inputs of an imported Sitefile are only known while building.",
    flags: FLAGS,
    options: OPTS,
    run: run_hosts,
};

fn used_by(u: &HostUse) -> String {
    let what = match u.kind {
        "import" => format!("IMPORT {}", u.name),
        "image" => format!("FROM image {}", u.name),
        _ => "BASE".to_string(),
    };
    match &u.span {
        Some(s) => format!("{what} ({}:{})", s.file, s.line),
        None => what,
    }
}

/// The text report: one row per use, then what each refused host or secret needs, and a summary line.
pub fn format_hosts(r: &HostsReport) -> String {
    let mut s = String::new();
    if r.hosts.is_empty() {
        s.push_str("No remote inputs: the Sitefile names no host to contact\n");
    } else {
        let mut rows: Vec<Vec<String>> = vec![["HOST", "PORT", "STATUS", "USED BY"]
            .iter()
            .map(|x| x.to_string())
            .collect()];
        for h in &r.hosts {
            for u in &h.uses {
                rows.push(vec![
                    h.host.clone(),
                    h.port.to_string(),
                    h.status.as_str().to_string(),
                    used_by(u),
                ]);
            }
        }
        s.push_str(&table(&rows));
        for h in &r.hosts {
            if let Some(n) = &h.needs {
                s.push_str(&format!("{}: needs {} {}\n", h.host, n.flag(), n.name));
            }
        }
        for u in r.hosts.iter().flat_map(|h| &h.uses) {
            if let Some(n) = &u.needs {
                s.push_str(&format!(
                    "{}: secret needs {} {}\n",
                    used_by(u),
                    n.flag(),
                    n.name
                ));
            }
        }
        for u in r.hosts.iter().flat_map(|h| &h.uses) {
            if u.nested {
                s.push_str(&format!(
                    "{}: an imported Sitefile; the hosts it needs are known once it is fetched\n",
                    used_by(u)
                ));
            }
        }
        let n = r.hosts.len();
        let counts: Vec<String> = [HostStatus::Allowed, HostStatus::Blocked, HostStatus::Denied]
            .iter()
            .map(|st| (st, r.hosts.iter().filter(|h| h.status == *st).count()))
            .filter(|(_, k)| *k > 0)
            .map(|(st, k)| format!("{k} {}", st.as_str()))
            .collect();
        s.push_str(&format!(
            "{n} host{}{}: {}\n",
            if n == 1 { "" } else { "s" },
            if r.strict {
                " (strict network mode)"
            } else {
                ""
            },
            counts.join(", ")
        ));
    }
    for line in &r.checklinks_external {
        s.push_str(&format!(
            "CHECKLINKS --external (line {line}) also contacts the hosts of external links\n"
        ));
    }
    s
}

fn run_hosts(v: &Parsed) -> Result<i32> {
    require_args(&v.positionals, 0, 1, "a context directory")?;
    let network = super::network::network_option(v)?;
    let mut o = BuildOptions {
        context_dir: v.positionals.first().cloned().unwrap_or_else(|| ".".into()),
        sitefile: v.str("file").filter(|s| !s.is_empty()).map(str::to_string),
        build_args: collect_build_args(&v.strs("build-arg-file"), &v.strs("build-arg"))?,
        base_image: v.str("base").map(str::to_string),
        ..Default::default()
    };
    o.fetch_policy.network = network;
    let warning: Callback = Arc::new(|m: &str| err(&format!("warning: {m}\n")));
    o.on_warning = Some(warning);
    let r = hosts(&o)?;
    if v.flag("json") {
        out(&(pretty_json(&r.to_json()) + "\n"));
    } else {
        out(&format_hosts(&r));
    }
    Ok(0)
}

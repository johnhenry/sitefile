//! `push` / `pull` (port of `src/cli/registry.ts`).
//!
//! The CLI owns argument checks, the `--username`/`--password-stdin` rules and
//! reading stdin; `crate::registry::{push, pull}` do the work and print events.

use super::args::{flag, string, strings, OptSpec, Parsed};
use super::common::{is_safe_integer, js_to_number, require_args, usage, Command};
use crate::error::Result;
use crate::refs::parse_ref;
use crate::registry::{
    pull, push, Credentials, PullOptions, PushOptions, RegistryLimits, RegistryOptions,
};
use crate::store::Store;

const COMMON_FLAGS_PUSH: &[&str] = &[
    "--insecure-registry <host[:port]>   allow plain http to this registry (repeatable; localhost always may;",
    "                                    also $SITEFILE_INSECURE_REGISTRIES, comma-separated)",
    "--username <name>                   registry user (with --password-stdin); otherwise credentials come from",
    "                                    $SITEFILE_REGISTRY_AUTH or ~/.docker/config.json (auths, credHelpers, credsStore)",
    "--password-stdin                    read the password from stdin",
    "--allow-private-network             let redirects/token servers on other hosts resolve to private addresses",
    "--timeout <ms>                      per-request idle timeout (default 30000; --read-timeout wins)",
    "--proxy <url>                       send requests through this HTTP proxy: http://[user:password@]host[:port]",
    "                                    (also $SITEFILE_PROXY; HTTP(S)_PROXY are never read)",
    "--no-proxy <hosts>                  hosts reached directly, not through the proxy (NO_PROXY syntax; repeatable,",
    "                                    comma-separated; also $SITEFILE_NO_PROXY)",
    "--allow-hosts <hosts>               contact only hosts matching these patterns (repeatable, comma-separated;",
    "                                    also $SITEFILE_ALLOW_HOSTS)",
    "--deny-hosts <hosts>                never contact hosts matching these patterns; beats --allow-hosts",
    "                                    (repeatable, comma-separated; also $SITEFILE_DENY_HOSTS)",
    "--network <mode>                    open (default) or strict: every host must match --allow-hosts, even with none",
    "                                    given (also $SITEFILE_NETWORK)",
    "--allow-secret-host <id=host>       in strict mode, let IMPORT ... SECRET <id> send its secret to this host",
    "                                    (a host pattern; repeatable)",
    "--connect-timeout <ms>              time limit for opening each connection (also $SITEFILE_FETCH_TIMEOUT)",
    "--read-timeout <ms>                 longest wait for data on a connection (also $SITEFILE_FETCH_TIMEOUT)",
];

const COMMON_FLAGS_PULL: &[&str] = &[
    "--platform <os/arch>   variant to choose from an image index (default linux/amd64)",
    "--max-download-bytes <n>   per-blob size limit (default 2 GiB)",
    "--insecure-registry <host[:port]>   allow plain http to this registry (repeatable; localhost always may;",
    "                                    also $SITEFILE_INSECURE_REGISTRIES, comma-separated)",
    "--username <name>                   registry user (with --password-stdin); otherwise credentials come from",
    "                                    $SITEFILE_REGISTRY_AUTH or ~/.docker/config.json (auths, credHelpers, credsStore)",
    "--password-stdin                    read the password from stdin",
    "--allow-private-network             let redirects/token servers on other hosts resolve to private addresses",
    "--timeout <ms>                      per-request idle timeout (default 30000; --read-timeout wins)",
    "--proxy <url>                       send requests through this HTTP proxy: http://[user:password@]host[:port]",
    "                                    (also $SITEFILE_PROXY; HTTP(S)_PROXY are never read)",
    "--no-proxy <hosts>                  hosts reached directly, not through the proxy (NO_PROXY syntax; repeatable,",
    "                                    comma-separated; also $SITEFILE_NO_PROXY)",
    "--allow-hosts <hosts>               contact only hosts matching these patterns (repeatable, comma-separated;",
    "                                    also $SITEFILE_ALLOW_HOSTS)",
    "--deny-hosts <hosts>                never contact hosts matching these patterns; beats --allow-hosts",
    "                                    (repeatable, comma-separated; also $SITEFILE_DENY_HOSTS)",
    "--network <mode>                    open (default) or strict: every host must match --allow-hosts, even with none",
    "                                    given (also $SITEFILE_NETWORK)",
    "--allow-secret-host <id=host>       in strict mode, let IMPORT ... SECRET <id> send its secret to this host",
    "                                    (a host pattern; repeatable)",
    "--connect-timeout <ms>              time limit for opening each connection (also $SITEFILE_FETCH_TIMEOUT)",
    "--read-timeout <ms>                 longest wait for data on a connection (also $SITEFILE_FETCH_TIMEOUT)",
];

const PUSH_OPTS: &[OptSpec] = &[
    strings("insecure-registry"),
    string("username"),
    flag("password-stdin"),
    flag("allow-private-network"),
    string("timeout"),
    string("proxy"),
    strings("no-proxy"),
    strings("allow-hosts"),
    strings("deny-hosts"),
    string("network"),
    strings("allow-secret-host"),
    string("connect-timeout"),
    string("read-timeout"),
];
const PULL_OPTS: &[OptSpec] = &[
    strings("insecure-registry"),
    string("username"),
    flag("password-stdin"),
    flag("allow-private-network"),
    string("timeout"),
    string("proxy"),
    strings("no-proxy"),
    strings("allow-hosts"),
    strings("deny-hosts"),
    string("network"),
    strings("allow-secret-host"),
    string("connect-timeout"),
    string("read-timeout"),
    string("platform"),
    string("max-download-bytes"),
];

/// Registries reachable over plain http: `--insecure-registry` flags plus $SITEFILE_INSECURE_REGISTRIES.
pub fn insecure_registries_from_env(extra: &[String]) -> Vec<String> {
    crate::registry::insecure_registries_from_env(extra, &crate::registry::process_env())
}

fn positive(raw: Option<&str>, flag: &str) -> Result<Option<u64>> {
    let Some(raw) = raw else { return Ok(None) };
    let n = js_to_number(raw);
    if !is_safe_integer(n) || n <= 0.0 {
        return usage(format!("{flag} expects a positive integer"));
    }
    Ok(Some(n as u64))
}

fn read_stdin() -> Result<String> {
    use std::io::Read;
    let mut buf = Vec::new();
    let _ = std::io::stdin().take(64 * 1024 + 1).read_to_end(&mut buf);
    if buf.len() > 64 * 1024 {
        return usage("--password-stdin: input is too long");
    }
    let s = String::from_utf8_lossy(&buf).into_owned();
    let s = s
        .strip_suffix('\n')
        .map(|t| t.strip_suffix('\r').unwrap_or(t))
        .unwrap_or(&s)
        .to_string();
    Ok(s)
}

/// The client options and explicit credentials, like `clientOptions` in registry.ts.
fn client_options(v: &Parsed) -> Result<(RegistryOptions, Option<Credentials>)> {
    let username = v.str("username").filter(|u| !u.is_empty());
    let mut explicit = None;
    if v.flag("password-stdin") {
        let Some(user) = username else {
            return usage("--password-stdin needs --username");
        };
        explicit = Some(Credentials::basic(user, read_stdin()?));
    } else if username.is_some() {
        return usage(
            "--username needs --password-stdin (passwords are not accepted on the command line)",
        );
    }
    let timeout = positive(v.str("timeout"), "--timeout")?;
    let max_blob = positive(v.str("max-download-bytes"), "--max-download-bytes")?;
    let network = super::network::network_option(v)?;
    let mut limits = RegistryLimits::default();
    if let Some(t) = timeout {
        limits.timeout_ms = t;
    }
    if let Some(m) = max_blob {
        limits.max_blob_bytes = m;
    }
    let options = RegistryOptions {
        insecure_registries: insecure_registries_from_env(&v.strs("insecure-registry")),
        allow_private_network: v.flag("allow-private-network"),
        limits,
        network,
        ..Default::default()
    };
    Ok((options, explicit))
}

fn print_line(l: &str) {
    super::common::out(&format!("{l}\n"));
}

pub const PUSH: Command = Command {
    name: "push",
    summary: "push an image to a registry",
    usage: "push <registry/name[:tag]> [options]",
    description: "Push the local image of that name to an OCI registry (distribution spec v1.1): blobs the registry lacks\nare uploaded (layers of a BASE image are mounted from its repository when it is on the same registry),\nthen the manifest. Name the image first: sitefile tag blog:1 localhost:5000/blog:1.\nThe registry host is always explicit (there is no Docker Hub default for push: use docker.io/user/name).\nPlain http is allowed for localhost and --insecure-registry hosts only.",
    flags: COMMON_FLAGS_PUSH,
    options: PUSH_OPTS,
    run: run_push,
};

fn run_push(v: &Parsed) -> Result<i32> {
    require_args(&v.positionals, 1, 1, "an image reference")?;
    let input = &v.positionals[0];
    let r = parse_ref(input)?;
    if r.registry.is_none() {
        return usage(format!("'{input}' has no registry host; tag the image first, e.g. sitefile tag {input} localhost:5000/{input}"));
    }
    let (registry, explicit_credentials) = client_options(v)?;
    let store = Store::open_default()?;
    push(
        &store,
        input,
        &PushOptions {
            registry,
            explicit_credentials,
            env: None,
            on_event: Some(&print_line),
        },
    )?;
    Ok(0)
}

pub const PULL: Command = Command {
    name: "pull",
    summary: "pull an image from a registry",
    usage: "pull <[registry/]name[:tag][@sha256:...]> [options]",
    description: "Pull an image into the local store and tag it name:tag as written. A name without a registry host\nmeans Docker Hub (nginx -> docker.io/library/nginx). For a multi-platform image the --platform variant is\nchosen (default linux/amd64). Every blob is sha256-verified while it streams in, and a manifest fetched by\ndigest must hash to it. The result is stored as-is (Docker or OCI manifests); only images built by\nsitefile can be served or saved. Pin with @sha256:... for reproducibility.",
    flags: COMMON_FLAGS_PULL,
    options: PULL_OPTS,
    run: run_pull,
};

fn run_pull(v: &Parsed) -> Result<i32> {
    require_args(&v.positionals, 1, 1, "an image reference")?;
    let input = &v.positionals[0];
    parse_ref(input)?;
    let store = Store::open_default()?;
    let (registry, explicit_credentials) = client_options(v)?;
    pull(
        &store,
        input,
        &PullOptions {
            registry,
            platform: v.str("platform").map(str::to_string),
            explicit_credentials,
            env: None,
            on_event: Some(&print_line),
        },
    )?;
    Ok(0)
}

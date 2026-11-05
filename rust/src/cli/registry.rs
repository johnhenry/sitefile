//! `push` / `pull` (port of `src/cli/registry.ts`).
//!
//! REGISTRY-HOOK: the registry client (`registry.rs`, `base_image.rs`) is ported
//! on the `port-registry` branch. Until it is merged both commands check their
//! arguments exactly like the TypeScript CLI and then fail with E_UNSUPPORTED;
//! connect them in `push_image` / `pull_image` below.

use super::args::{flag, string, strings, OptSpec, Parsed};
use super::common::{js_to_number, is_safe_integer, require_args, usage, Command};
use crate::error::{ErrorCode, Result, SitefileError};
use crate::refs::{parse_ref, with_default_registry, ImageRef};

const COMMON_FLAGS_PUSH: &[&str] = &[
    "--insecure-registry <host[:port]>   allow plain http to this registry (repeatable; localhost always may;",
    "                                    also $SITEFILE_INSECURE_REGISTRIES, comma-separated)",
    "--username <name>                   registry user (with --password-stdin); otherwise credentials come from",
    "                                    $SITEFILE_REGISTRY_AUTH or ~/.docker/config.json (auths, credHelpers, credsStore)",
    "--password-stdin                    read the password from stdin",
    "--allow-private-network             let redirects/token servers on other hosts resolve to private addresses",
    "--timeout <ms>                      per-request idle timeout (default 30000)",
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
    "--timeout <ms>                      per-request idle timeout (default 30000)",
];

const PUSH_OPTS: &[OptSpec] = &[strings("insecure-registry"), string("username"), flag("password-stdin"), flag("allow-private-network"), string("timeout")];
const PULL_OPTS: &[OptSpec] = &[
    strings("insecure-registry"),
    string("username"),
    flag("password-stdin"),
    flag("allow-private-network"),
    string("timeout"),
    string("platform"),
    string("max-download-bytes"),
];

/// Registries reachable over plain http: `--insecure-registry` flags plus $SITEFILE_INSECURE_REGISTRIES.
pub fn insecure_registries_from_env(extra: &[String]) -> Vec<String> {
    let env = std::env::var("SITEFILE_INSECURE_REGISTRIES").unwrap_or_default();
    let mut out: Vec<String> = Vec::new();
    for s in extra.iter().cloned().chain(env.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty())) {
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

fn positive(raw: Option<&str>, flag: &str) -> Result<Option<u64>> {
    let Some(raw) = raw else { return Ok(None) };
    let n = js_to_number(raw);
    if !is_safe_integer(n) || n <= 0.0 {
        return usage(format!("{flag} expects a positive integer"));
    }
    Ok(Some(n as u64))
}

/// Check the credential and limit flags like `clientOptions` in registry.ts.
fn check_client_options(v: &Parsed) -> Result<()> {
    let username = v.str("username");
    if v.flag("password-stdin") {
        if username.is_none_or(str::is_empty) {
            return usage("--password-stdin needs --username");
        }
    } else if username.is_some_and(|u| !u.is_empty()) {
        return usage("--username needs --password-stdin (passwords are not accepted on the command line)");
    }
    positive(v.str("timeout"), "--timeout")?;
    positive(v.str("max-download-bytes"), "--max-download-bytes")?;
    Ok(())
}

fn not_yet(what: &str) -> SitefileError {
    SitefileError::plain(ErrorCode::E_UNSUPPORTED, format!("{what} is not yet available in the Rust build"))
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
    check_client_options(v)?;
    push_image(&r, v)
}

/// REGISTRY-HOOK: push the local image named `r` (see src/cli/registry.ts).
fn push_image(_r: &ImageRef, _v: &Parsed) -> Result<i32> {
    Err(not_yet("push"))
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
    let r = with_default_registry(&parse_ref(&v.positionals[0])?);
    check_client_options(v)?;
    pull_image(&r, v)
}

/// REGISTRY-HOOK: pull `r` into the local store (see src/cli/registry.ts).
fn pull_image(_r: &ImageRef, _v: &Parsed) -> Result<i32> {
    Err(not_yet("pull"))
}

//! The network flags (port of `src/cli/network.ts`).

use super::args::Parsed;
use crate::error::{ErrorCode::E_USAGE, Result, SitefileError};
use crate::resources::network::{
    merge_network, network_from_env, network_problem, parse_network_mode, split_list,
    NetworkSettings,
};

/// Help lines for the network flags, at the given flag-column width.
pub fn network_help(width: usize) -> Vec<String> {
    let rows: [(&str, &[&str]); 8] = [
        (
            "--proxy <url>",
            &[
                "send requests through this HTTP proxy: http://[user:password@]host[:port]",
                "(also $SITEFILE_PROXY; HTTP(S)_PROXY are never read)",
            ],
        ),
        (
            "--no-proxy <hosts>",
            &[
                "hosts reached directly, not through the proxy (NO_PROXY syntax; repeatable,",
                "comma-separated; also $SITEFILE_NO_PROXY)",
            ],
        ),
        (
            "--allow-hosts <hosts>",
            &[
                "contact only hosts matching these patterns (repeatable, comma-separated;",
                "also $SITEFILE_ALLOW_HOSTS)",
            ],
        ),
        (
            "--deny-hosts <hosts>",
            &[
                "never contact hosts matching these patterns; beats --allow-hosts",
                "(repeatable, comma-separated; also $SITEFILE_DENY_HOSTS)",
            ],
        ),
        (
            "--network <mode>",
            &[
                "open (default) or strict: every host must match --allow-hosts, even with none",
                "given (also $SITEFILE_NETWORK)",
            ],
        ),
        (
            "--allow-secret-host <id=host>",
            &[
                "in strict mode, let IMPORT ... SECRET <id> send its secret to this host",
                "(a host pattern; repeatable)",
            ],
        ),
        (
            "--connect-timeout <ms>",
            &["time limit for opening each connection (also $SITEFILE_FETCH_TIMEOUT)"],
        ),
        (
            "--read-timeout <ms>",
            &["longest wait for data on a connection (also $SITEFILE_FETCH_TIMEOUT)"],
        ),
    ];
    let mut out = Vec::new();
    for (flag, lines) in rows {
        // A flag wider than the column gets a line of its own.
        let own_line = flag.len() >= width;
        if own_line {
            out.push(flag.to_string());
        }
        for (i, l) in lines.iter().enumerate() {
            let f = if i == 0 && !own_line { flag } else { "" };
            out.push(format!("{f:<width$}{l}"));
        }
    }
    out
}

fn ms(raw: Option<&str>, flag: &str) -> Result<Option<u64>> {
    let Some(raw) = raw else { return Ok(None) };
    let ok = !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit());
    match raw.parse::<u64>() {
        Ok(n) if ok && n > 0 && n <= 9_007_199_254_740_991 => Ok(Some(n)),
        _ => Err(SitefileError::plain(
            E_USAGE,
            format!("{flag} expects a positive integer"),
        )),
    }
}

/// The network settings for a command: the SITEFILE_* environment, then the
/// flags on top (flags replace the proxy and timeouts and add to the host
/// lists). E_USAGE for a bad value.
pub fn network_option(v: &Parsed) -> Result<NetworkSettings> {
    let list =
        |name: &str| -> Vec<String> { v.strs(name).iter().flat_map(|s| split_list(s)).collect() };
    let flags = NetworkSettings {
        proxy: v.str("proxy").map(str::to_string),
        no_proxy: list("no-proxy"),
        allow_hosts: list("allow-hosts"),
        deny_hosts: list("deny-hosts"),
        strict: false,
        secret_hosts: list("allow-secret-host"),
        connect_timeout_ms: ms(v.str("connect-timeout"), "--connect-timeout")?,
        read_timeout_ms: ms(v.str("read-timeout"), "--read-timeout")?,
    };
    let mut env = network_from_env(&crate::registry::process_env())?;
    // --network replaces SITEFILE_NETWORK (--network open turns an inherited strict mode off).
    let mut flags = flags;
    if let Some(raw) = v.str("network") {
        let mode = parse_network_mode(if raw.is_empty() { "?" } else { raw }, "--network")?;
        env.strict = false;
        flags.strict = mode == Some(true);
    }
    let merged = merge_network(&env, &flags);
    if let Some(problem) = network_problem(&merged) {
        return Err(SitefileError::plain(E_USAGE, problem));
    }
    Ok(merged)
}

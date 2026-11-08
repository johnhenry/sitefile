//! The network flags (port of `src/cli/network.ts`).

use super::args::Parsed;
use crate::error::{ErrorCode::E_USAGE, Result, SitefileError};
use crate::resources::network::{
    merge_network, network_from_env, network_problem, split_list, NetworkSettings,
};

/// Help lines for the network flags, at the given flag-column width.
pub fn network_help(width: usize) -> Vec<String> {
    let rows: [(&str, &[&str]); 6] = [
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
        for (i, l) in lines.iter().enumerate() {
            out.push(format!("{:<width$}{l}", if i == 0 { flag } else { "" }));
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
        connect_timeout_ms: ms(v.str("connect-timeout"), "--connect-timeout")?,
        read_timeout_ms: ms(v.str("read-timeout"), "--read-timeout")?,
    };
    let env = network_from_env(&crate::registry::process_env())?;
    let merged = merge_network(&env, &flags);
    if let Some(problem) = network_problem(&merged) {
        return Err(SitefileError::plain(E_USAGE, problem));
    }
    Ok(merged)
}

//! `cache info` / `cache prune` (port of `src/cli/cache.ts`).

use super::args::{flag, string, OptSpec, Parsed};
use super::common::{human_size, js_to_number, out, pretty_json, require_args, usage, Command};
use crate::error::Result;
use crate::plugins::cache::TransformCache;
use crate::store::resolve_store_home;
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;

/// Where TRANSFORM results are cached: `$SITEFILE_HOME/cache/transform`.
pub fn transform_cache_dir() -> PathBuf {
    resolve_store_home().join("cache").join("transform")
}

const INFO_OPTS: &[OptSpec] = &[flag("json")];
pub const INFO: Command = Command {
    name: "cache info",
    summary: "show the TRANSFORM result cache location and size",
    usage: "cache info [--json]",
    description: "Print where plugin/tool results are cached, and how many entries and bytes it holds.",
    flags: &["    --json   machine-readable output"],
    options: INFO_OPTS,
    run: run_info,
};

fn run_info(v: &Parsed) -> Result<i32> {
    require_args(&v.positionals, 0, 0, "no arguments")?;
    let i = TransformCache::new(transform_cache_dir()).info();
    let dir = i.dir.display().to_string();
    if v.flag("json") {
        out(&(pretty_json(&json!({ "dir": dir, "entries": i.entries, "blobs": i.blobs, "bytes": i.bytes })) + "\n"));
    } else {
        out(&format!("Cache:   {dir}\nEntries: {}\nBlobs:   {}\nSize:    {}\n", i.entries, i.blobs, human_size(i.bytes)));
    }
    Ok(0)
}

const PRUNE_OPTS: &[OptSpec] = &[string("older-than")];
pub const PRUNE: Command = Command {
    name: "cache prune",
    summary: "delete cached TRANSFORM results",
    usage: "cache prune [--older-than <days>]",
    description: "Delete every cached result, or only entries not used for the given number of days, and the blobs no entry still uses.",
    flags: &["    --older-than <days>   keep entries used within this many days"],
    options: PRUNE_OPTS,
    run: run_prune,
};

fn run_prune(v: &Parsed) -> Result<i32> {
    require_args(&v.positionals, 0, 0, "no arguments")?;
    let mut older = Duration::ZERO;
    if let Some(raw) = v.str("older-than") {
        let days = js_to_number(raw);
        if !days.is_finite() || days <= 0.0 {
            return usage("--older-than expects a positive number of days");
        }
        older = Duration::from_secs_f64(days * 86_400.0);
    }
    let r = TransformCache::new(transform_cache_dir()).prune(older);
    out(&format!(
        "Deleted {} entr{} and {} blob{}\nTotal reclaimed space: {}\n",
        r.entries,
        if r.entries == 1 { "y" } else { "ies" },
        r.blobs,
        if r.blobs == 1 { "" } else { "s" },
        human_size(r.bytes)
    ));
    Ok(0)
}

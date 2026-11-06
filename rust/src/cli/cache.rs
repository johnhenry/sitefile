//! `cache info` / `cache prune` (port of `src/cli/cache.ts`).

use super::args::{flag, string, OptSpec, Parsed};
use super::common::{err, human_size, js_to_number, out, pretty_json, require_args, table, usage, Command};
use crate::cache_limit::{cache_limit_from_env, cache_usage, enforce_cache_limit, format_limit, parse_size, CACHE_MAX_ENV};
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

const PRUNE_OPTS: &[OptSpec] = &[string("older-than"), string("max-size")];
pub const PRUNE: Command = Command {
    name: "cache prune",
    summary: "delete cached TRANSFORM results",
    usage: "cache prune [--older-than <days> | --max-size <size>]",
    description: "Delete every cached TRANSFORM result, or only entries not used for the given number of days, and the blobs no\nentry still uses. With --max-size, instead apply the automatic size limit now with that size: least recently\nused evictable data (TRANSFORM results, compiled wasm modules, untagged images and unused blobs) is deleted\nuntil the rest fits. Tagged images are never touched.",
    flags: &["    --older-than <days>   keep entries used within this many days", "    --max-size <size>     shrink evictable data to this size (e.g. 500M, 2G)"],
    options: PRUNE_OPTS,
    run: run_prune,
};

fn run_prune(v: &Parsed) -> Result<i32> {
    require_args(&v.positionals, 0, 0, "no arguments")?;
    if let Some(max) = v.str("max-size") {
        if v.str("older-than").is_some() {
            return usage("--older-than and --max-size conflict");
        }
        let Some(n) = parse_size(max).filter(|n| *n > 0) else {
            return usage("--max-size expects a size greater than 0, such as 500M or 2G");
        };
        let r = enforce_cache_limit(&resolve_store_home(), n)?;
        out(&format!("Evicted {} item{}\nTotal reclaimed space: {}\n", r.items, if r.items == 1 { "" } else { "s" }, human_size(r.bytes)));
        return Ok(0);
    }
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

const DU_OPTS: &[OptSpec] = &[flag("json")];
pub const DU: Command = Command {
    name: "cache du",
    summary: "show the size of every cache under $SITEFILE_HOME",
    usage: "cache du [--json]",
    description: "Print the size of each cache and store under $SITEFILE_HOME, how much of it may be evicted automatically\n(EVICTABLE), and the size limit ($SITEFILE_CACHE_MAX). Alias: cache ls.",
    flags: &["    --json   machine-readable output (sizes in bytes; limit 0 = unlimited)"],
    options: DU_OPTS,
    run: run_du,
};

fn run_du(v: &Parsed) -> Result<i32> {
    require_args(&v.positionals, 0, 0, "no arguments")?;
    let home = resolve_store_home();
    let limit = cache_limit_from_env(&mut |m| err(&format!("warning: {m}\n")));
    let caches = cache_usage(&home)?;
    let bytes: u64 = caches.iter().map(|c| c.bytes).sum();
    let evictable: u64 = caches.iter().map(|c| c.evictable).sum();
    if v.flag("json") {
        let list: Vec<serde_json::Value> = caches
            .iter()
            .map(|c| json!({ "name": c.name, "path": c.path.display().to_string(), "items": c.items, "bytes": c.bytes, "evictable": c.evictable }))
            .collect();
        out(&(pretty_json(&json!({ "home": home.display().to_string(), "limit": limit, "caches": list, "bytes": bytes, "evictable": evictable })) + "\n"));
        return Ok(0);
    }
    let mut rows = vec![vec!["CACHE".to_string(), "ITEMS".into(), "SIZE".into(), "EVICTABLE".into(), "PATH".into()]];
    for c in &caches {
        rows.push(vec![c.name.to_string(), c.items.to_string(), human_size(c.bytes), human_size(c.evictable), c.path.display().to_string()]);
    }
    rows.push(vec!["total".into(), String::new(), human_size(bytes), human_size(evictable), home.display().to_string()]);
    out(&table(&rows));
    out(&format!("Limit: {} ({CACHE_MAX_ENV}; evictable data over it is deleted after each build, least recently used first)\n", format_limit(limit)));
    Ok(0)
}

/// End of `build`/`inspect`: apply `$SITEFILE_CACHE_MAX`. Never fails: problems are warnings.
pub fn evict_after_build() {
    let warn = |m: &str| err(&format!("warning: {m}\n"));
    let limit = cache_limit_from_env(&mut |m| warn(m));
    if limit == 0 {
        return;
    }
    match enforce_cache_limit(&resolve_store_home(), limit) {
        Ok(r) if r.items > 0 => err(&format!(
            "Evicted {} cache item{} ({}) to stay under {CACHE_MAX_ENV}={}\n",
            r.items,
            if r.items == 1 { "" } else { "s" },
            human_size(r.bytes),
            format_limit(limit)
        )),
        Ok(_) => {}
        Err(e) => warn(&format!("cache size limit not applied: {}", e.message())),
    }
}

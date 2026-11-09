//! `sitefile up`, `down` and `ps` (port of `src/cli/up.ts`, issue #223).

use super::args::{flag, string, string_s, OptSpec, Parsed};
use super::common::{err, out, pretty_json, require_args, table, Command};
use super::servers::grace_of;
use crate::error::{ErrorCode::*, Result, SitefileError};
use crate::server::format_url;
use crate::servers::{
    archive_source, is_regular_file, is_running, looks_like_dir, parse_publish, resolve_source,
    restart_detached, serve_detached_with, stop_server, Bind, DetachedServe, ServerRegistry,
    ServerSource, ServerState,
};
use crate::servers_file::{find_servers_file, load_servers_file, ServersFile};
use crate::store::Store;
use serde_json::{json, Value};
use std::path::Path;

/// A servers-file source: a path (`./dist`, `../site.zip`, `/abs`) relative to
/// the file's directory; otherwise an image reference or ID, else an archive
/// file by that name next to the file.
fn file_source(store: &Store, dir: &Path, src: &str) -> Result<(String, ServerSource)> {
    if looks_like_dir(src) {
        let p = crate::export::exporter::resolve_path(&dir.join(src));
        return resolve_source(store, &p.display().to_string());
    }
    match store.resolve(src) {
        Ok(id) => Ok((src.to_string(), ServerSource::Image { image_id: id })),
        Err(e) => {
            let file = crate::export::exporter::resolve_path(&dir.join(src))
                .display()
                .to_string();
            if e.code == E_NOT_FOUND && is_regular_file(&file) {
                return Ok(archive_source(&file));
            }
            Err(e)
        }
    }
}

/// The servers file and its real path (the `project` its servers are recorded with).
fn open(v: &Parsed) -> Result<(ServersFile, String)> {
    let name = find_servers_file(v.str("file"))?;
    let file = load_servers_file(&name)?;
    let real = std::fs::canonicalize(&name)
        .map_err(|_| {
            SitefileError::plain(E_NOT_FOUND, format!("cannot read servers file '{name}'"))
        })?
        .display()
        .to_string();
    Ok((file, real))
}

const FILE_FLAG: &str =
    "-f, --file <path>   servers file (default: sitefile.servers.yaml, .yml or .json here)";
const UP_FLAGS: &[&str] = &[
    FILE_FLAG,
    "--time <secs>       grace period before SIGKILL when a server is replaced (default 5)",
];
const DOWN_FLAGS: &[&str] = &[
    FILE_FLAG,
    "--time <secs>       grace period before SIGKILL (default 5)",
];
const PS_FLAGS: &[&str] = &[FILE_FLAG, "--json              machine-readable output"];
const UP_OPTS: &[OptSpec] = &[string_s("file", 'f'), string("time")];
const PS_OPTS: &[OptSpec] = &[string_s("file", 'f'), flag("json")];

pub const UP: Command = Command {
    name: "up",
    summary: "start the servers of a servers file (detached; idempotent)",
    usage: "up [-f sitefile.servers.yaml] [--time secs]",
    description: "Start every server of the servers file in the background, as serve -d would, named by its key. Running again\nis safe: a server that is running with the same source, address and options is left alone, an exited one is\nrestarted, and one whose entry changed is replaced. Every source is checked before anything starts. A server\nwith the same name that this file did not start is refused (remove it with \"sitefile rm <name>\").",
    flags: UP_FLAGS,
    options: UP_OPTS,
    run: run_up,
};

pub const DOWN: Command = Command {
    name: "down",
    summary: "stop and remove the servers a servers file started",
    usage: "down [-f sitefile.servers.yaml] [--time secs]",
    description: "Stop (SIGTERM, then SIGKILL after --time seconds) and remove every server that \"sitefile up\" started from the\nservers file, also ones no longer listed in it. Servers started otherwise are not touched.",
    flags: DOWN_FLAGS,
    options: UP_OPTS,
    run: run_down,
};

pub const PS: Command = Command {
    name: "ps",
    summary: "list the servers of a servers file",
    usage: "ps [-f sitefile.servers.yaml] [--json]",
    description: "List the servers of the servers file in file order: running, exited, or not created (never started by \"sitefile up\"),\nthen servers it started that are no longer listed in it.",
    flags: PS_FLAGS,
    options: PS_OPTS,
    run: run_ps,
};

fn same_config(cur: &ServerState, w: &DetachedServe) -> bool {
    cur.source == w.source
        && cur.spa == w.spa
        && cur.keep_os_metadata == w.keep_os_metadata
        && cur.bind == w.bind
}

fn run_up(v: &Parsed) -> Result<i32> {
    require_args(&v.positionals, 0, 0, "no arguments")?;
    let grace = grace_of(v)?;
    let (file, project) = open(v)?;
    let store = Store::open_default()?;
    // Resolve everything first: a typo or a missing directory starts nothing.
    let mut wanted: Vec<DetachedServe> = Vec::new();
    for s in &file.servers {
        let (label, source) = file_source(&store, &file.dir, &s.source).map_err(|e| {
            SitefileError::new(
                e.code,
                format!("{}: server '{}': {}", file.label, s.name, e.detail),
                e.span.clone(),
            )
        })?;
        let bind: Bind = parse_publish(s.publish.as_deref()).map_err(|e| {
            SitefileError::plain(
                E_DATA,
                format!("{}: server '{}': publish: {}", file.label, s.name, e.detail),
            )
        })?;
        wanted.push(DetachedServe {
            label,
            source,
            name: Some(s.name.clone()),
            bind,
            spa: s.spa,
            keep_os_metadata: false,
            allowed_hosts: Vec::new(),
            cors: false,
            isolate: None,
            project: Some(project.clone()),
        });
    }
    let reg = ServerRegistry::open(Some(store))?;
    let all = reg.all()?;
    for w in &wanted {
        let name = w.name.as_deref().unwrap_or_default();
        if all
            .iter()
            .any(|s| s.name == name && s.project.as_deref() != Some(project.as_str()))
        {
            return Err(SitefileError::plain(
                E_SERVER,
                format!(
                    "{}: a server named '{name}' already exists and was not started from this file; remove it with \"sitefile rm {name}\"",
                    file.label
                ),
            ));
        }
    }
    let mut failed = false;
    for w in &wanted {
        let name = w.name.clone().unwrap_or_default();
        let cur = all.iter().find(|s| s.name == name);
        let r = (|| -> Result<()> {
            if let Some(cur) = cur.filter(|c| same_config(c, w)) {
                if is_running(cur) {
                    out(&format!(
                        "{} ({}) is up to date at {}\n",
                        cur.name,
                        cur.id,
                        format_url(&cur.host, cur.port)
                    ));
                    return Ok(());
                }
                let again = restart_detached(&reg, cur, grace, None)?;
                out(&format!(
                    "Restarted {} ({}) serving {} at {}\n",
                    again.name,
                    again.id,
                    again.image,
                    format_url(&again.host, again.port)
                ));
                return Ok(());
            }
            if let Some(cur) = cur {
                stop_server(&reg, cur, grace)?;
                reg.remove(&cur.id)?;
            }
            let started = serve_detached_with(&reg, w, None, &|m| err(&format!("warning: {m}\n")))?;
            out(&format!(
                "{} {} ({}) serving {} at {}\n",
                if cur.is_some() {
                    "Recreated"
                } else {
                    "Started"
                },
                started.name,
                started.id,
                w.label,
                format_url(&started.host, started.port)
            ));
            Ok(())
        })();
        if let Err(e) = r {
            err(&format!("error[{}]: {name}: {}\n", e.code, e.message()));
            failed = true;
        }
    }
    for s in &all {
        if s.project.as_deref() == Some(project.as_str())
            && !wanted
                .iter()
                .any(|w| w.name.as_deref() == Some(s.name.as_str()))
        {
            err(&format!(
                "warning: {} was started from {} but is no longer in it; \"sitefile down\" stops it\n",
                s.name, file.label
            ));
        }
    }
    Ok(if failed { 1 } else { 0 })
}

/// The records this file started: its servers in file order, then any no longer in it (oldest first).
fn of_project<'a>(
    all: &'a [ServerState],
    file: &ServersFile,
    project: &str,
) -> Vec<&'a ServerState> {
    let mine: Vec<&ServerState> = all
        .iter()
        .filter(|s| s.project.as_deref() == Some(project))
        .collect();
    let mut out: Vec<&ServerState> = Vec::new();
    for s in &file.servers {
        for m in &mine {
            if m.name == s.name {
                out.push(m);
            }
        }
    }
    for m in &mine {
        if !out.iter().any(|o| o.id == m.id) {
            out.push(m);
        }
    }
    out
}

fn run_down(v: &Parsed) -> Result<i32> {
    require_args(&v.positionals, 0, 0, "no arguments")?;
    let grace = grace_of(v)?;
    let (file, project) = open(v)?;
    let reg = ServerRegistry::open(None)?;
    let all = reg.all()?;
    let mut failed = false;
    for s in of_project(&all, &file, &project) {
        let r = stop_server(&reg, s, grace).and_then(|_| reg.remove(&s.id));
        match r {
            Ok(()) => out(&format!("Removed {} ({})\n", s.name, s.id)),
            Err(e) => {
                err(&format!("error[{}]: {}: {}\n", e.code, s.name, e.message()));
                failed = true;
            }
        }
    }
    Ok(if failed { 1 } else { 0 })
}

struct Row {
    name: String,
    site: String,
    url: String,
    status: String,
    id: Option<String>,
    listed: bool,
}

fn run_ps(v: &Parsed) -> Result<i32> {
    require_args(&v.positionals, 0, 0, "no arguments")?;
    let (file, project) = open(v)?;
    let reg = ServerRegistry::open(None)?;
    let all = reg.all()?;
    let mut rows: Vec<Row> = Vec::new();
    for s in &file.servers {
        match all
            .iter()
            .find(|x| x.name == s.name && x.project.as_deref() == Some(project.as_str()))
        {
            None => rows.push(Row {
                name: s.name.clone(),
                site: s.source.clone(),
                url: String::new(),
                status: "not created".into(),
                id: None,
                listed: true,
            }),
            Some(rec) => {
                let view = reg.view(rec);
                rows.push(Row {
                    name: s.name.clone(),
                    site: rec.image.clone(),
                    url: view.url,
                    status: view.status.into(),
                    id: Some(rec.id.clone()),
                    listed: true,
                });
            }
        }
    }
    for rec in of_project(&all, &file, &project) {
        if file.servers.iter().any(|s| s.name == rec.name) {
            continue;
        }
        let view = reg.view(rec);
        rows.push(Row {
            name: rec.name.clone(),
            site: rec.image.clone(),
            url: view.url,
            status: view.status.into(),
            id: Some(rec.id.clone()),
            listed: false,
        });
    }
    if v.flag("json") {
        let arr: Vec<Value> = rows
            .iter()
            .map(|r| {
                json!({
                    "name": r.name,
                    "site": r.site,
                    "url": r.url,
                    "status": r.status,
                    "id": r.id,
                    "listed": r.listed,
                })
            })
            .collect();
        out(&(pretty_json(&Value::Array(arr)) + "\n"));
        return Ok(0);
    }
    let mut t: Vec<Vec<String>> = vec![["NAME", "SITE", "URL", "STATUS", "ID"]
        .iter()
        .map(|s| s.to_string())
        .collect()];
    for r in &rows {
        t.push(vec![
            r.name.clone(),
            r.site.clone(),
            if r.url.is_empty() {
                "-".into()
            } else {
                r.url.clone()
            },
            if r.listed {
                r.status.clone()
            } else {
                format!("{} (not in the file)", r.status)
            },
            r.id.clone().unwrap_or_else(|| "-".into()),
        ]);
    }
    out(&table(&t));
    Ok(0)
}

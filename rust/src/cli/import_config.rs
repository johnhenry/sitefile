//! `sitefile import-config` (port of `src/cli/import-config.ts`, issue #204).

use super::args::{flag, string, OptSpec, Parsed};
use super::common::{err, out, pretty_json, require_args, usage, Command};
use crate::error::{ErrorCode::*, Result, SitefileError};
use crate::host_config::{
    format_of, headers_text, import_host_config, redirects_text, HOST_FORMATS,
};
use serde_json::{json, Value};

const OPTS: &[OptSpec] = &[
    string("format"),
    flag("redirects"),
    flag("headers"),
    flag("json"),
];

const FLAGS: &[&str] = &[
    "--format <f>    netlify, vercel, firebase, azure, wrangler, htaccess",
    "--redirects     print only the _redirects text",
    "--headers       print only the _headers text",
    "--json          machine-readable output (rules and unsupported entries)",
];

pub const IMPORT_CONFIG: Command = Command {
    name: "import-config",
    summary: "translate a host's config file into _redirects and _headers",
    usage: "import-config <file> [--format f] [--redirects | --headers] [--json]",
    description: "Read netlify.toml, vercel.json, firebase.json, staticwebapp.config.json, wrangler.toml / wrangler.json\n([assets]) or .htaccess, and print the _redirects and _headers that sitefile serve understands today.\nEvery rule printed is one serve applies as written; anything else (placeholders, conditions, proxying,\nregular expressions, settings with no _redirects form) is listed on stderr as a warning, never half\ntranslated. The format comes from the file name unless --format is given.",
    flags: FLAGS,
    options: OPTS,
    run: run_import_config,
};

fn run_import_config(v: &Parsed) -> Result<i32> {
    require_args(&v.positionals, 1, 1, "a host config file")?;
    let file = &v.positionals[0];
    let formats = HOST_FORMATS.join(", ");
    let format = match v.str("format") {
        Some(f) if HOST_FORMATS.contains(&f) => f.to_string(),
        Some(f) => return usage(format!("--format expects one of {formats}, got '{f}'")),
        None => match format_of(file) {
            Some(f) => f.to_string(),
            None => {
                return usage(format!(
                    "cannot tell the format of '{file}' from its name; pass --format ({formats})"
                ))
            }
        },
    };
    if v.flag("redirects") && v.flag("headers") {
        return usage("--redirects and --headers conflict");
    }
    if v.flag("json") && (v.flag("redirects") || v.flag("headers")) {
        return usage("--json prints both; drop --redirects/--headers");
    }
    let text = std::fs::read_to_string(file)
        .map_err(|_| SitefileError::plain(E_NOT_FOUND, format!("cannot read '{file}'")))?;
    let c = import_host_config(&text, file, &format)?;
    if v.flag("json") {
        let headers: Vec<Value> = c
            .headers
            .iter()
            .map(|b| {
                json!({
                    "pattern": b.pattern,
                    "headers": b.headers.iter().map(|(n, v)| json!([n, v])).collect::<Vec<_>>(),
                })
            })
            .collect();
        let unsupported: Vec<Value> = c
            .unsupported
            .iter()
            .map(|u| json!({ "entry": u.entry, "reason": u.reason }))
            .collect();
        let doc = json!({
            "format": c.format,
            "redirects": c.redirects,
            "headers": headers,
            "unsupported": unsupported,
        });
        out(&(pretty_json(&doc) + "\n"));
        return Ok(0);
    }
    for u in &c.unsupported {
        err(&format!("warning: {file}: {}: {}\n", u.entry, u.reason));
    }
    let r = redirects_text(&c);
    let h = headers_text(&c);
    if v.flag("redirects") {
        out(&r);
    } else if v.flag("headers") {
        out(&h);
    } else {
        let mut s = String::new();
        if !r.is_empty() {
            s.push_str("# _redirects\n");
            s.push_str(&r);
        }
        if !r.is_empty() && !h.is_empty() {
            s.push('\n');
        }
        if !h.is_empty() {
            s.push_str("# _headers\n");
            s.push_str(&h);
        }
        out(&s);
    }
    Ok(0)
}

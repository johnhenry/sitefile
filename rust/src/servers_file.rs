//! The servers file for `sitefile up`, `down` and `ps` (port of
//! `src/servers-file.ts`, issue #223): several detached site servers described
//! in one place, like a Compose file.
//!
//! ```yaml
//! servers:
//!   docs:
//!     source: ./docs/dist     # a directory or archive file (relative to this file), or an image
//!     publish: 8081           # [host:]port, as serve -p (default 127.0.0.1, a free port)
//!     spa: true               # as serve --spa (default false)
//! ```

use crate::error::{ErrorCode::*, Result, SitefileError};
use crate::values::js_string_of;
use crate::yaml::parse_yaml;
use serde_json::Value;
use std::path::{Path, PathBuf};

pub const SERVERS_FILES: [&str; 3] = [
    "sitefile.servers.yaml",
    "sitefile.servers.yml",
    "sitefile.servers.json",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSpec {
    pub name: String,
    /// As written: `./dir`, `../site.zip`, `/abs`, an image reference or ID, or an archive file name.
    pub source: String,
    /// `[host:]port` as written (a number in its decimal form); None: 127.0.0.1 and a free port.
    pub publish: Option<String>,
    pub spa: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServersFile {
    /// The file as given (for messages).
    pub label: String,
    /// The directory relative sources are resolved against.
    pub dir: PathBuf,
    pub servers: Vec<ServerSpec>,
}

const SERVER_KEYS: [&str; 3] = ["source", "publish", "spa"];

fn fail<T>(label: &str, detail: impl AsRef<str>) -> Result<T> {
    Err(SitefileError::plain(
        E_DATA,
        format!("{label}: {}", detail.as_ref()),
    ))
}

/// `^[A-Za-z0-9][A-Za-z0-9_.-]{0,62}$`
fn is_name(n: &str) -> bool {
    let b = n.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
}

/// `file` when given, else the first of [`SERVERS_FILES`] in the current directory.
pub fn find_servers_file(file: Option<&str>) -> Result<String> {
    if let Some(f) = file {
        return Ok(f.to_string());
    }
    for name in SERVERS_FILES {
        if std::fs::read(name).is_ok() {
            return Ok(name.to_string());
        }
    }
    Err(SitefileError::plain(
        E_NOT_FOUND,
        format!(
            "no servers file: {} not found in the current directory (or pass -f)",
            SERVERS_FILES.join(", ")
        ),
    ))
}

/// Check the shape of a parsed servers file.
pub fn parse_servers_definition(value: &Value, label: &str, dir: &Path) -> Result<ServersFile> {
    let Value::Object(top) = value else {
        return fail(label, "a servers file must be a mapping with a servers key");
    };
    if let Some(k) = top.keys().find(|k| *k != "servers") {
        return fail(label, format!("unknown key '{k}' (expected servers)"));
    }
    let raw = match top.get("servers") {
        Some(Value::Object(m)) if !m.is_empty() => m,
        _ => return fail(label, "servers must be a non-empty mapping of name: server"),
    };
    let mut servers = Vec::new();
    for (name, s) in raw {
        if !is_name(name) {
            return fail(
                label,
                format!("'{name}' is not a valid server name (letters, digits, '_', '.', '-'; at most 63)"),
            );
        }
        // A key of digits only would be reordered by JavaScript objects; file order is the start order.
        if name.bytes().all(|b| b.is_ascii_digit()) {
            return fail(
                label,
                format!("'{name}' is not a valid server name here: use at least one letter"),
            );
        }
        let at = format!("server '{name}'");
        let Value::Object(m) = s else {
            return fail(label, format!("{at} must be a mapping with a source"));
        };
        if let Some(k) = m.keys().find(|k| !SERVER_KEYS.contains(&k.as_str())) {
            return fail(
                label,
                format!(
                    "{at}: unknown key '{k}' (expected {})",
                    SERVER_KEYS.join(", ")
                ),
            );
        }
        let source = match m.get("source") {
            Some(Value::String(x)) if !x.is_empty() => x.clone(),
            _ => return fail(label, format!("{at}: source must be a non-empty string")),
        };
        let publish = match m.get("publish") {
            None | Some(Value::Null) => None,
            Some(Value::String(x)) => Some(x.clone()),
            Some(n @ Value::Number(num))
                if num.as_f64().is_some_and(|f| {
                    f >= 0.0 && f.fract() == 0.0 && f <= 9_007_199_254_740_991.0
                }) =>
            {
                Some(js_string_of(n))
            }
            _ => {
                return fail(
                    label,
                    format!("{at}: publish must be [host:]port (a string or a port number)"),
                )
            }
        };
        let spa = match m.get("spa") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            _ => return fail(label, format!("{at}: spa must be true or false")),
        };
        servers.push(ServerSpec {
            name: name.clone(),
            source,
            publish,
            spa,
        });
    }
    Ok(ServersFile {
        label: label.to_string(),
        dir: dir.to_path_buf(),
        servers,
    })
}

/// Read and check a servers file (.yaml, .yml or .json).
pub fn load_servers_file(file: &str) -> Result<ServersFile> {
    let base = Path::new(file)
        .file_name()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let json = base.ends_with(".json");
    if !json && !base.ends_with(".yaml") && !base.ends_with(".yml") {
        return Err(SitefileError::plain(
            E_USAGE,
            format!("servers file '{file}': unknown format (use a .yaml, .yml or .json file)"),
        ));
    }
    let text = std::fs::read_to_string(file).map_err(|_| {
        SitefileError::plain(E_NOT_FOUND, format!("cannot read servers file '{file}'"))
    })?;
    let value = if json {
        serde_json::from_str::<Value>(text.strip_prefix('\u{feff}').unwrap_or(&text))
            .map_err(|e| SitefileError::plain(E_DATA, format!("{file} is not valid JSON: {e}")))?
    } else {
        parse_yaml(&text, file)?
    };
    let abs = crate::export::exporter::resolve_path(Path::new(file));
    let dir = abs.parent().unwrap_or(Path::new("/")).to_path_buf();
    parse_servers_definition(&value, file, &dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Result<ServersFile> {
        parse_servers_definition(
            &serde_json::from_str(json).unwrap(),
            "f.yaml",
            Path::new("/d"),
        )
    }

    #[test]
    fn parses_servers_in_file_order() {
        let f = parse(r#"{"servers":{"b":{"source":"./b","publish":8081,"spa":true},"a":{"source":"img:1","publish":"0.0.0.0:80"}}}"#).unwrap();
        assert_eq!(
            f.servers,
            vec![
                ServerSpec {
                    name: "b".into(),
                    source: "./b".into(),
                    publish: Some("8081".into()),
                    spa: true
                },
                ServerSpec {
                    name: "a".into(),
                    source: "img:1".into(),
                    publish: Some("0.0.0.0:80".into()),
                    spa: false
                },
            ]
        );
    }

    #[test]
    fn rejects_bad_shapes() {
        for (json, msg) in [
            ("[]", "f.yaml: a servers file must be a mapping with a servers key"),
            (r#"{"x":1}"#, "f.yaml: unknown key 'x' (expected servers)"),
            (r#"{"servers":{}}"#, "f.yaml: servers must be a non-empty mapping of name: server"),
            (r#"{"servers":{"-a":{"source":"x"}}}"#, "f.yaml: '-a' is not a valid server name (letters, digits, '_', '.', '-'; at most 63)"),
            (r#"{"servers":{"12":{"source":"x"}}}"#, "f.yaml: '12' is not a valid server name here: use at least one letter"),
            (r#"{"servers":{"a":"x"}}"#, "f.yaml: server 'a' must be a mapping with a source"),
            (r#"{"servers":{"a":{"source":"x","port":1}}}"#, "f.yaml: server 'a': unknown key 'port' (expected source, publish, spa)"),
            (r#"{"servers":{"a":{}}}"#, "f.yaml: server 'a': source must be a non-empty string"),
            (r#"{"servers":{"a":{"source":"x","publish":1.5}}}"#, "f.yaml: server 'a': publish must be [host:]port (a string or a port number)"),
            (r#"{"servers":{"a":{"source":"x","spa":"yes"}}}"#, "f.yaml: server 'a': spa must be true or false"),
        ] {
            let e = parse(json).unwrap_err();
            assert_eq!((e.code, e.detail.as_str()), (E_DATA, msg), "{json}");
        }
    }
}

//! Build secrets (issue #85; port of `src/secrets.ts`), modelled on Docker's
//! `--secret id=…,env=…|src=…`: values that remote IMPORTs send as HTTP request headers
//! (`IMPORT … SECRET <id> AS HEADER <Name>` / `AS BEARER`).
//!
//! A secret value lives only in memory: it is sent to the IMPORT URL's own origin (never after a
//! redirect to another one) and never becomes part of the output, a cache or memo key,
//! Sitefile.lock, a vendor manifest, provenance, a --profile trace, a warning or an error message.
//! As a last line of defence, warnings and errors of a build that has secrets are passed through
//! [`redact_secrets`].

use crate::error::{ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::language::ast::is_ident;
use crate::planner::SecretRef;

/// Secrets by id, in the order they were given.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Secrets(Vec<(String, String)>);

impl Secrets {
    pub fn new(pairs: Vec<(String, String)>) -> Secrets {
        Secrets(pairs)
    }
    pub fn get(&self, id: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == id)
            .map(|(_, v)| v.as_str())
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(k, _)| k.as_str())
    }
}

fn usage(message: impl Into<String>) -> SitefileError {
    SitefileError::plain(E_USAGE, message)
}

/// Visible ASCII, space and tab: what every HTTP client sends unchanged in a header value.
fn header_value_ok(v: &str) -> bool {
    !v.is_empty() && v.chars().all(|c| c == '\t' || (' '..='~').contains(&c))
}

/// Parse `--secret` flags: `id=<id>,env=<VAR>` or `id=<id>,src=<file>` (relative to the working
/// directory; one trailing newline is dropped). Messages never include a value.
pub fn load_secrets(flags: &[String], env: &dyn Fn(&str) -> Option<String>) -> Result<Secrets> {
    let mut out: Vec<(String, String)> = Vec::new();
    for flag in flags {
        let mut fields: Vec<(&str, &str)> = Vec::new();
        for part in flag.split(',') {
            let (key, value) = match part.split_once('=') {
                Some((k, v)) if k == "id" || k == "env" || k == "src" => (k, v),
                // never echo the flag: it might be the secret itself
                _ => {
                    return Err(usage(
                        "--secret expects id=<id>,env=<VAR> or id=<id>,src=<file>",
                    ))
                }
            };
            if fields.iter().any(|(k, _)| *k == key) {
                return Err(usage(format!("--secret: '{key}' given twice")));
            }
            fields.push((key, value));
        }
        let field = |k: &str| fields.iter().find(|(f, _)| *f == k).map(|(_, v)| *v);
        let id = match field("id") {
            Some(id) if is_ident(id) => id.to_string(),
            _ => {
                return Err(usage(
                    "--secret needs id=<id> (letters, digits and _, not starting with a digit)",
                ))
            }
        };
        if out.iter().any(|(k, _)| *k == id) {
            return Err(usage(format!("--secret id={id} given twice")));
        }
        let value = match (field("env"), field("src")) {
            (Some(name), None) => env(name).ok_or_else(|| {
                SitefileError::plain(
                    E_SECRET,
                    format!("--secret id={id}: environment variable {name} is not set"),
                )
            })?,
            (None, Some(src)) => {
                let bytes = std::fs::read(src).map_err(|_| {
                    SitefileError::plain(E_SECRET, format!("--secret id={id}: cannot read '{src}'"))
                })?;
                // latin1, like Buffer.toString('latin1'): one char per byte.
                let mut s: String = bytes.iter().map(|&b| b as char).collect();
                if s.ends_with("\r\n") {
                    s.truncate(s.len() - 2);
                } else if s.ends_with('\n') {
                    s.truncate(s.len() - 1);
                }
                s
            }
            _ => {
                return Err(usage(format!(
                    "--secret id={id}: give exactly one of env=<VAR> or src=<file>"
                )))
            }
        };
        if value.is_empty() {
            return Err(SitefileError::plain(
                E_SECRET,
                format!("--secret id={id} is empty"),
            ));
        }
        if !header_value_ok(&value) {
            return Err(SitefileError::plain(
                E_SECRET,
                format!("--secret id={id} cannot be sent in an HTTP header (only printable ASCII, spaces and tabs)"),
            ));
        }
        out.push((id, value));
    }
    Ok(Secrets(out))
}

/// Read process environment variables (as UTF-8; anything else counts as invalid below).
pub fn process_env(name: &str) -> Option<String> {
    std::env::var_os(name).map(|v| v.to_string_lossy().into_owned())
}

/// Replace every occurrence of a secret value (longest first) with `***`.
pub fn redact_secrets(text: &str, secrets: &Secrets) -> String {
    if secrets.is_empty() {
        return text.to_string();
    }
    let mut values: Vec<&str> = secrets.0.iter().map(|(_, v)| v.as_str()).collect();
    values.sort_by(|a, b| b.chars().count().cmp(&a.chars().count()));
    let mut out = text.to_string();
    for v in values {
        if !v.is_empty() {
            out = out.split(v).collect::<Vec<_>>().join("***");
        }
    }
    out
}

/// The error with the secrets redacted from its message.
pub fn redact_error(e: SitefileError, secrets: &Secrets) -> SitefileError {
    if secrets.is_empty() {
        return e;
    }
    SitefileError {
        detail: redact_secrets(&e.detail, secrets),
        ..e
    }
}

/// The request headers of an import's SECRET clauses (lowercase names); a secret that was not given is E_SECRET.
pub fn secret_headers(
    name: &str,
    refs: &[SecretRef],
    span: &SourceSpan,
    secrets: &Secrets,
) -> Result<Vec<(String, String)>> {
    let mut headers: Vec<(String, String)> = Vec::new();
    for s in refs {
        let Some(v) = secrets.get(&s.id) else {
            return Err(SitefileError::at(
                E_SECRET,
                format!(
                    "IMPORT {name}: secret '{}' was not given; pass --secret id={},env=<VAR> or --secret id={},src=<file>",
                    s.id, s.id, s.id
                ),
                span,
            ));
        };
        let value = if s.bearer {
            format!("Bearer {v}")
        } else {
            v.to_string()
        };
        let key = s.header.to_lowercase();
        match headers.iter_mut().find(|(k, _)| *k == key) {
            Some(h) => h.1 = value,
            None => headers.push((key, value)),
        }
    }
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(name: &str) -> Option<String> {
        match name {
            "API_TOKEN" => Some("tok-secret-value".into()),
            "EMPTY" => Some(String::new()),
            "CRLF" => Some("a\r\nX-Injected: 1".into()),
            _ => None,
        }
    }

    fn code(flags: &[&str]) -> String {
        let flags: Vec<String> = flags.iter().map(|s| s.to_string()).collect();
        match load_secrets(&flags, &env) {
            Ok(_) => "ok".into(),
            Err(e) => {
                assert!(!e.message().contains("tok-secret-value"), "{}", e.message());
                e.code.as_str().to_string()
            }
        }
    }

    #[test]
    fn parses_env_and_src_secrets() {
        let dir = std::env::temp_dir().join(format!("sitefile-secret-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("tok");
        std::fs::write(&file, "tok-secret-value\r\n").unwrap();
        let s = load_secrets(
            &[
                "id=a,env=API_TOKEN".into(),
                format!("id=b,src={}", file.display()),
            ],
            &env,
        )
        .unwrap();
        assert_eq!(s.get("a"), Some("tok-secret-value"));
        assert_eq!(s.get("b"), Some("tok-secret-value"));
        assert_eq!(s.ids().collect::<Vec<_>>(), ["a", "b"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn bad_flags_and_values_never_show_the_value() {
        assert_eq!(code(&["tok-secret-value"]), "E_USAGE");
        assert_eq!(code(&["id=a,value=tok-secret-value"]), "E_USAGE");
        assert_eq!(code(&["id=a"]), "E_USAGE");
        assert_eq!(code(&["id=1a,env=API_TOKEN"]), "E_USAGE");
        assert_eq!(code(&["id=a,env=X,src=y"]), "E_USAGE");
        assert_eq!(code(&["id=a,id=b,env=API_TOKEN"]), "E_USAGE");
        assert_eq!(
            code(&["id=a,env=API_TOKEN", "id=a,env=API_TOKEN"]),
            "E_USAGE"
        );
        assert_eq!(code(&["id=a,env=NOPE"]), "E_SECRET");
        assert_eq!(code(&["id=a,src=/nonexistent/secret/file"]), "E_SECRET");
        assert_eq!(code(&["id=a,env=EMPTY"]), "E_SECRET");
        assert_eq!(code(&["id=a,env=CRLF"]), "E_SECRET");
    }

    #[test]
    fn redaction_and_headers() {
        let s = Secrets::new(vec![
            ("k".into(), "abc".into()),
            ("t".into(), "abcdef".into()),
        ]);
        assert_eq!(redact_secrets("x abcdef y abc", &s), "x *** y ***");
        let e = redact_error(SitefileError::plain(E_FETCH, "GET /x?k=abc failed"), &s);
        assert_eq!(e.detail, "GET /x?k=*** failed");
        let span = SourceSpan {
            file: "Sitefile".into(),
            line: 1,
            column: 1,
        };
        let refs = [
            SecretRef {
                id: "k".into(),
                header: "X-Api-Key".into(),
                bearer: false,
            },
            SecretRef {
                id: "t".into(),
                header: "Authorization".into(),
                bearer: true,
            },
        ];
        assert_eq!(
            secret_headers("d", &refs, &span, &s).unwrap(),
            [
                ("x-api-key".to_string(), "abc".to_string()),
                ("authorization".to_string(), "Bearer abcdef".to_string())
            ]
        );
        let e = secret_headers("d", &refs, &span, &Secrets::default()).unwrap_err();
        assert_eq!(e.code, E_SECRET);
        assert_eq!(e.detail, "IMPORT d: secret 'k' was not given; pass --secret id=k,env=<VAR> or --secret id=k,src=<file>");
    }
}

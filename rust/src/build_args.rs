//! `--build-arg` values and `--build-arg-file` files (src/build-args.ts).

use crate::error::{ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::language::ast::is_ident;
use crate::language::scanner::{is_js_space, js_trim};
use crate::values::js_string_of;
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

use crate::yaml::parse_yaml;

/// Build-arg values by name, in JavaScript object key order: integer-like keys
/// first (ascending), then the rest in insertion order. Setting an existing name
/// replaces its value in place, like assigning to a JS object.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildArgs {
    entries: Vec<(String, String)>,
}

/// A canonical array index (`0`, `1`, … `4294967294`), which JS objects order first.
fn array_index(key: &str) -> Option<u64> {
    if key.is_empty() || key.len() > 10 || !key.bytes().all(|b| b.is_ascii_digit()) || (key.len() > 1 && key.starts_with('0')) {
        return None;
    }
    key.parse::<u64>().ok().filter(|n| *n < 4_294_967_295)
}

impl BuildArgs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let key = key.into();
        let value = value.into();
        match self.entries.iter_mut().find(|(k, _)| *k == key) {
            Some(e) => e.1 = value,
            None => self.entries.push((key, value)),
        }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// `Object.assign(self, other)`.
    pub fn extend_from(&mut self, other: &BuildArgs) {
        for (k, v) in other.iter() {
            self.set(k, v);
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Entries in JavaScript key order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        let mut indexed: Vec<(u64, &(String, String))> =
            self.entries.iter().filter_map(|e| array_index(&e.0).map(|n| (n, e))).collect();
        indexed.sort_by_key(|(n, _)| *n);
        indexed
            .into_iter()
            .map(|(_, e)| e)
            .chain(self.entries.iter().filter(|e| array_index(&e.0).is_none()))
            .map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.iter().map(|(k, _)| k)
    }
}

impl<K: Into<String>, V: Into<String>> FromIterator<(K, V)> for BuildArgs {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        let mut out = BuildArgs::new();
        for (k, v) in iter {
            out.set(k, v);
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildArgFileKind {
    Json,
    Yaml,
    Env,
}

/// `path.basename` (POSIX): the last segment, ignoring trailing slashes.
fn basename(file: &str) -> &str {
    let t = file.trim_end_matches('/');
    t.rsplit('/').next().unwrap_or(t)
}

/// The format of a build-arg file, by extension.
pub fn build_arg_file_kind(file: &str) -> Result<BuildArgFileKind> {
    let base = basename(file).to_lowercase();
    if base.ends_with(".json") {
        return Ok(BuildArgFileKind::Json);
    }
    if base.ends_with(".yaml") || base.ends_with(".yml") {
        return Ok(BuildArgFileKind::Yaml);
    }
    if base == ".env" || base.starts_with(".env.") || base.ends_with(".env") {
        return Ok(BuildArgFileKind::Env);
    }
    Err(SitefileError::plain(
        E_USAGE,
        format!("--build-arg-file '{file}': unknown format (use a .json, .yaml, .yml or .env file)"),
    ))
}

/// Lexically normalized absolute path (`path.resolve`, no filesystem access).
fn resolve(p: &Path, cwd: &Path) -> PathBuf {
    let joined = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `path.relative(process.cwd(), path.resolve(file)) || file`.
fn display_label(file: &str) -> String {
    let Ok(cwd) = std::env::current_dir() else { return file.to_string() };
    let cwd = resolve(&cwd, Path::new("/"));
    let target = resolve(Path::new(file), &cwd);
    let a: Vec<_> = cwd.components().collect();
    let b: Vec<_> = target.components().collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut parts: Vec<String> = vec!["..".to_string(); a.len() - common];
    parts.extend(b[common..].iter().map(|c| c.as_os_str().to_string_lossy().into_owned()));
    let rel = parts.join("/");
    if rel.is_empty() {
        file.to_string()
    } else {
        rel
    }
}

/// Read build-arg values from a file, by extension:
///
/// - `.json`: a flat object, `{"TITLE": "Docs", "PORT": 8080}`
/// - `.yaml` / `.yml`: a flat mapping, `TITLE: Docs`
/// - `.env` (or a file named `.env`, `.env.*`, `*.env`): `KEY=value` lines
///
/// Values must be strings, numbers or booleans; they become strings and are
/// checked against each ARG's declared type later, like `--build-arg`.
/// `.env` files are parsed, never executed: no `$VAR` or `$(…)` expansion.
pub fn load_build_arg_file(file: &str) -> Result<BuildArgs> {
    let kind = build_arg_file_kind(file)?; // an unknown extension is a usage error, before any IO
    let text = std::fs::read(file)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .map_err(|_| SitefileError::plain(E_NOT_FOUND, format!("cannot read build-arg file '{file}'")))?;
    let label = display_label(file);
    let value = match kind {
        BuildArgFileKind::Env => return parse_env_file(&text, &label),
        BuildArgFileKind::Json => serde_json::from_str::<Value>(text.strip_prefix('\u{feff}').unwrap_or(&text))
            .map_err(|e| SitefileError::plain(E_DATA, format!("{label} is not valid JSON: {e}")))?,
        BuildArgFileKind::Yaml => parse_yaml(&text, &label)?,
    };
    flat_scalars(&value, &label)
}

fn flat_scalars(value: &Value, label: &str) -> Result<BuildArgs> {
    let Value::Object(map) = value else {
        return Err(SitefileError::plain(E_DATA, format!("{label}: build-arg files must contain a mapping of NAME: value")));
    };
    // Visit keys in JavaScript order so the first error is the same one.
    let order: BuildArgs = map.keys().map(|k| (k.clone(), String::new())).collect();
    let mut out = BuildArgs::new();
    for key in order.keys() {
        if !is_ident(key) {
            return Err(SitefileError::plain(E_DATA, format!("{label}: '{key}' is not a valid ARG name")));
        }
        match &map[key] {
            v @ (Value::String(_) | Value::Number(_) | Value::Bool(_)) => out.set(key, js_string_of(v)),
            other => {
                let what = match other {
                    Value::Null => "null",
                    Value::Array(_) => "lists",
                    _ => "objects",
                };
                return Err(SitefileError::plain(
                    E_DATA,
                    format!("{label}: {key} must be a string, number or boolean (ARGs can't hold {what})"),
                ));
            }
        }
    }
    Ok(out)
}

/// `^\s*(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*=\s*` on one line: (key, match length).
fn env_key(raw: &[char]) -> Option<(String, usize)> {
    let skip_ws = |mut i: usize| {
        while i < raw.len() && is_js_space(raw[i]) {
            i += 1;
        }
        i
    };
    let rest_from = |i: usize| -> Option<(String, usize)> {
        let start = i;
        if !raw.get(i).is_some_and(|c| *c == '_' || c.is_ascii_alphabetic()) {
            return None;
        }
        let mut j = i + 1;
        while raw.get(j).is_some_and(|c| *c == '_' || c.is_ascii_alphanumeric()) {
            j += 1;
        }
        let key: String = raw[start..j].iter().collect();
        let k = skip_ws(j);
        (raw.get(k) == Some(&'=')).then(|| (key, skip_ws(k + 1)))
    };
    let i = skip_ws(0);
    let export: Vec<char> = "export".chars().collect();
    if raw[i..].starts_with(&export) {
        let j = i + export.len();
        let k = skip_ws(j);
        if k > j {
            if let Some(m) = rest_from(k) {
                return Some(m);
            }
        }
    }
    rest_from(i)
}

/// `.replace(/\s+#.*$/, '')`: drop the first whitespace-then-`#` and everything after it.
fn strip_inline_comment(s: &[char]) -> &[char] {
    for k in 0..s.len() {
        if !is_js_space(s[k]) || (k > 0 && is_js_space(s[k - 1])) {
            continue;
        }
        let mut e = k;
        while e < s.len() && is_js_space(s[e]) {
            e += 1;
        }
        if s.get(e) == Some(&'#') {
            return &s[..k];
        }
    }
    s
}

/// `.env` syntax: one `KEY=value` per line, optional `export ` prefix, `#`
/// comment lines and blank lines. Unquoted values are trimmed and a ` #` starts
/// a comment; `'single'` quotes are literal; `"double"` quotes understand
/// `\n \t \r \" \\` and may span lines. Nothing is expanded or executed.
pub fn parse_env_file(text: &str, label: &str) -> Result<BuildArgs> {
    let mut out = BuildArgs::new();
    let text = text.strip_prefix('\u{feff}').unwrap_or(text).replace("\r\n", "\n").replace('\r', "\n");
    let src: Vec<char> = text.chars().collect();
    let err = |line: u32, msg: String| SitefileError::at(E_DATA, msg, &SourceSpan::new(label, line, 1));
    let find_nl = |from: usize| src[from.min(src.len())..].iter().position(|c| *c == '\n').map(|k| k + from);
    let mut i = 0usize;
    let mut line = 1u32;
    while i < src.len() {
        let end = find_nl(i).unwrap_or(src.len());
        let raw = &src[i..end];
        let raw_s: String = raw.iter().collect();
        let trimmed = js_trim(&raw_s);
        let start_line = line;
        if trimmed.is_empty() || trimmed.starts_with('#') {
            i = end + 1;
            line += 1;
            continue;
        }
        let Some((key, matched)) = env_key(raw) else {
            return Err(err(line, format!("expected KEY=value, got '{trimmed}'")));
        };
        let mut j = i + matched;
        let value: String;
        match src.get(j).copied() {
            Some(q @ ('\'' | '"')) => {
                j += 1;
                let mut v = String::new();
                let mut closed = false;
                while j < src.len() {
                    let c = src[j];
                    if c == q {
                        closed = true;
                        j += 1;
                        break;
                    }
                    if c == '\n' {
                        line += 1;
                    }
                    if q == '"' && c == '\\' && j + 1 < src.len() {
                        v.push(match src[j + 1] {
                            'n' => '\n',
                            't' => '\t',
                            'r' => '\r',
                            n => n,
                        });
                        j += 2;
                        continue;
                    }
                    v.push(c);
                    j += 1;
                }
                if !closed {
                    return Err(err(start_line, format!("unterminated {q} quote for {key}")));
                }
                let rest_end = find_nl(j).unwrap_or(src.len());
                let rest: String = src[j..rest_end].iter().collect();
                let rest = js_trim(&rest);
                if !rest.is_empty() && !rest.starts_with('#') {
                    return Err(err(line, format!("unexpected text after the quoted value of {key}")));
                }
                i = rest_end + 1;
                value = v;
            }
            _ => {
                let v: String = strip_inline_comment(&src[j..end]).iter().collect();
                value = js_trim(&v).to_string();
                i = end + 1;
            }
        }
        line += 1;
        out.set(key, value);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(a: &BuildArgs) -> Vec<(String, String)> {
        a.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn expect(p: &[(&str, &str)]) -> Vec<(String, String)> {
        p.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn env_parsing() {
        let src = [
            "# comment",
            "",
            "export TITLE=My site  # trailing comment",
            "SINGLE='literal $HOME \\n'",
            "DOUBLE=\"line1\\nline2 \\\"q\\\"\"",
            "MULTI=\"a",
            "b\"",
            "EMPTY=",
            "URL=https://example.com/#anchor",
            "CMD=$(rm -rf /)",
            "  SPACED = value ",
        ]
        .join("\n");
        let env = parse_env_file(&src, ".env").unwrap();
        assert_eq!(
            pairs(&env),
            expect(&[
                ("TITLE", "My site"),
                ("SINGLE", "literal $HOME \\n"),
                ("DOUBLE", "line1\nline2 \"q\""),
                ("MULTI", "a\nb"),
                ("EMPTY", ""),
                ("URL", "https://example.com/#anchor"),
                ("CMD", "$(rm -rf /)"),
                ("SPACED", "value"),
            ])
        );
    }

    #[test]
    fn env_edge_cases() {
        let env = parse_env_file("\u{feff}export=1\r\nexport  X = \"a\" # c\rexportY=2\nA=1\nA=2\n", ".env").unwrap();
        assert_eq!(pairs(&env), expect(&[("export", "1"), ("X", "a"), ("exportY", "2"), ("A", "2")]));
        let e = parse_env_file("export FOO\n", ".env").unwrap_err();
        assert_eq!(e.detail, "expected KEY=value, got 'export FOO'");
    }

    #[test]
    fn env_errors_carry_the_line() {
        let at = |src: &str, line: u32| {
            let e = parse_env_file(src, ".env").unwrap_err();
            assert_eq!((e.code, e.span.as_ref().map(|s| s.line)), (E_DATA, Some(line)), "{src}: {e}");
            e.message()
        };
        assert_eq!(at("A=1\nnot a pair\n", 2), ".env:2:1: expected KEY=value, got 'not a pair'");
        assert_eq!(at("A=1\nB=\"open\n\n", 2), ".env:2:1: unterminated \" quote for B");
        assert_eq!(at("A=\"x\" junk\n", 1), ".env:1:1: unexpected text after the quoted value of A");
        at("1BAD=x\n", 1);
        at("A=\"x\ny\" junk\n", 2);
    }

    #[test]
    fn file_kinds() {
        assert_eq!(build_arg_file_kind("vars.json").unwrap(), BuildArgFileKind::Json);
        assert_eq!(build_arg_file_kind("cfg/args.YAML").unwrap(), BuildArgFileKind::Yaml);
        assert_eq!(build_arg_file_kind("x.yml").unwrap(), BuildArgFileKind::Yaml);
        assert_eq!(build_arg_file_kind(".env").unwrap(), BuildArgFileKind::Env);
        assert_eq!(build_arg_file_kind(".env.production").unwrap(), BuildArgFileKind::Env);
        assert_eq!(build_arg_file_kind("prod.env").unwrap(), BuildArgFileKind::Env);
        let e = build_arg_file_kind("vars.toml").unwrap_err();
        assert_eq!(e.code, E_USAGE);
        assert_eq!(e.detail, "--build-arg-file 'vars.toml': unknown format (use a .json, .yaml, .yml or .env file)");
    }

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("sitefile-build-args-{}-{:?}", std::process::id(), std::thread::current().id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn json_files_flat_scalars_only() {
        let d = tmpdir();
        let write = |name: &str, body: &str| {
            let p = d.join(name);
            std::fs::write(&p, body).unwrap();
            p.to_string_lossy().into_owned()
        };
        let a = write("a.json", "\u{feff}{\"TITLE\":\"Docs\",\"PORT\":8080,\"DEBUG\":false,\"F\":1.5}");
        assert_eq!(
            pairs(&load_build_arg_file(&a).unwrap()),
            expect(&[("TITLE", "Docs"), ("PORT", "8080"), ("DEBUG", "false"), ("F", "1.5")])
        );
        for (name, body, msg) in [
            ("nested.json", "{\"A\":{\"b\":1}}", "A must be a string, number or boolean (ARGs can't hold objects)"),
            ("list.json", "[1,2]", "build-arg files must contain a mapping of NAME: value"),
            ("nul.json", "{\"A\":null}", "(ARGs can't hold null)"),
            ("arr.json", "{\"A\":[1]}", "(ARGs can't hold lists)"),
            ("badkey.json", "{\"not valid\":1}", "'not valid' is not a valid ARG name"),
            ("order.json", "{\"A\":{},\"1\":2}", "'1' is not a valid ARG name"),
            ("broken.json", "{", "is not valid JSON: "),
        ] {
            let e = load_build_arg_file(&write(name, body)).unwrap_err();
            assert_eq!(e.code, E_DATA, "{name}: {e}");
            assert!(e.detail.contains(msg), "{name}: {e}");
            assert!(e.detail.contains(name), "{name}: label in {e}");
        }
        let env = write("x.env", "A=1\n");
        assert_eq!(pairs(&load_build_arg_file(&env).unwrap()), expect(&[("A", "1")]));
        let missing = d.join("missing.json").to_string_lossy().into_owned();
        let e = load_build_arg_file(&missing).unwrap_err();
        assert_eq!(e.code, E_NOT_FOUND);
        assert_eq!(e.detail, format!("cannot read build-arg file '{missing}'"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn js_key_order() {
        let a: BuildArgs = [("b", "1"), ("10", "x"), ("a", "2"), ("2", "y"), ("01", "z"), ("b", "3")].into_iter().collect();
        assert_eq!(a.keys().collect::<Vec<_>>(), ["2", "10", "b", "a", "01"]);
        assert_eq!(a.get("b"), Some("3"));
        let mut c = BuildArgs::new();
        c.set("a", "0");
        c.extend_from(&a);
        assert_eq!(c.get("a"), Some("2"));
        assert_eq!(c.len(), 5);
    }

    #[test]
    fn labels_are_relative_to_cwd() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(display_label(&cwd.join("x/y.json").to_string_lossy()), "x/y.json");
        assert_eq!(display_label("./a/../b.env"), "b.env");
        assert_eq!(display_label("."), ".");
    }
}

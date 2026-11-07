//! `sitefile bake`: build several variants from one bake file (port of
//! `src/bake.ts`). This module reads and resolves the bake file into a
//! [`BakePlan`]; `cli/bake.rs` runs the builds.

use crate::error::{ErrorCode::*, Result, SitefileError};
use crate::executor::{join_path, normalize_posix};
use crate::export::formats::default_out_name;
use crate::language::ast::ExportFormat;
use crate::values::js_string_of;
use crate::yaml::parse_yaml;
use serde_json::{json, Map, Value};

/// The default bake file names, looked up in this order in the current directory.
pub const BAKE_FILES: &[&str] = &[
    "sitefile.bake.yaml",
    "sitefile.bake.yml",
    "sitefile.bake.json",
];

const TARGET_KEYS: &str =
    "context, file, args, arg-files, target, output, format, tags, matrix, inherits";

/// One build of a bake plan. Paths are as the CLI passes them to `build`.
#[derive(Debug, Clone, PartialEq)]
pub struct BakeTarget {
    pub name: String,
    pub context: String,
    pub file: String,
    /// Build args in definition order (inherited ones first).
    pub args: Vec<(String, String)>,
    pub arg_files: Vec<String>,
    pub target: Option<String>,
    pub output: String,
    pub format: Option<String>,
    pub tags: Vec<String>,
    pub matrix: Option<Vec<(String, String)>>,
}

#[derive(Debug, Clone)]
pub struct BakePlan {
    pub file: String,
    pub targets: Vec<BakeTarget>,
}

#[derive(Debug, Clone, Default)]
struct Fields {
    context: Option<String>,
    file: Option<String>,
    args: Option<Vec<(String, String)>>,
    arg_files: Option<Vec<String>>,
    target: Option<String>,
    output: Option<String>,
    format: Option<String>,
    tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default)]
struct RawTarget {
    fields: Fields,
    matrix: Option<Vec<(String, Vec<String>)>>,
    inherits: Vec<String>,
}

pub struct Definition {
    label: String,
    targets: Vec<(String, RawTarget)>,
    groups: Vec<(String, Vec<String>)>,
}

impl Definition {
    fn target(&self, name: &str) -> Option<&RawTarget> {
        self.targets.iter().find(|(n, _)| n == name).map(|(_, t)| t)
    }
    fn group(&self, name: &str) -> Option<&Vec<String>> {
        self.groups.iter().find(|(n, _)| n == name).map(|(_, g)| g)
    }
}

fn fail<T>(label: &str, detail: impl AsRef<str>) -> Result<T> {
    Err(SitefileError::plain(
        E_DATA,
        format!("{label}: {}", detail.as_ref()),
    ))
}

/// `/^[A-Za-z0-9_][A-Za-z0-9._-]*$/`
fn is_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// `/^[A-Za-z_][A-Za-z0-9_]*$/`
fn is_key(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn is_scalar(v: &Value) -> bool {
    matches!(v, Value::String(_) | Value::Number(_) | Value::Bool(_))
}

/// Find the bake file: `file` when given, else the first of [`BAKE_FILES`] in the current directory.
pub fn find_bake_file(file: Option<&str>) -> Result<String> {
    if let Some(f) = file {
        return Ok(f.to_string());
    }
    for name in BAKE_FILES {
        if std::fs::read(name).is_ok() {
            return Ok(name.to_string());
        }
    }
    Err(SitefileError::plain(
        E_NOT_FOUND,
        format!(
            "no bake file: {} not found in the current directory (or pass -f)",
            BAKE_FILES.join(", ")
        ),
    ))
}

/// `path.basename` (POSIX).
fn basename(file: &str) -> &str {
    let t = file.trim_end_matches('/');
    t.rsplit('/').next().unwrap_or(t)
}

/// `path.dirname` (POSIX).
fn dirname(file: &str) -> String {
    let t = if file.len() > 1 {
        file.trim_end_matches('/')
    } else {
        file
    };
    match t.rfind('/') {
        None => ".".into(),
        Some(0) => "/".into(),
        Some(i) => t[..i].to_string(),
    }
}

/// Read and parse a bake file (no resolution yet).
pub fn load_bake_file(file: &str) -> Result<Value> {
    let base = basename(file).to_lowercase();
    let json = base.ends_with(".json");
    if !json && !base.ends_with(".yaml") && !base.ends_with(".yml") {
        return Err(SitefileError::plain(
            E_USAGE,
            format!("bake file '{file}': unknown format (use a .yaml, .yml or .json file)"),
        ));
    }
    let text = std::fs::read_to_string(file).map_err(|_| {
        SitefileError::plain(E_NOT_FOUND, format!("cannot read bake file '{file}'"))
    })?;
    if !json {
        return parse_yaml(&text, file);
    }
    serde_json::from_str::<Value>(text.strip_prefix('\u{feff}').unwrap_or(&text))
        .map_err(|e| SitefileError::plain(E_DATA, format!("{file} is not valid JSON: {e}")))
}

fn string_field(label: &str, at: &str, key: &str, v: &Value) -> Result<String> {
    match v {
        Value::String(s) => Ok(s.clone()),
        _ => fail(label, format!("{at}: {key} must be a string")),
    }
}

fn string_list(label: &str, at: &str, key: &str, v: &Value) -> Result<Vec<String>> {
    match v {
        Value::Array(a) if a.iter().all(|x| x.is_string()) => {
            Ok(a.iter().map(js_string_of).collect())
        }
        _ => fail(label, format!("{at}: {key} must be a list of strings")),
    }
}

fn parse_target(label: &str, name: &str, v: &Value) -> Result<RawTarget> {
    let at = format!("target '{name}'");
    let mut t = RawTarget::default();
    let map = match v {
        Value::Null => return Ok(t),
        Value::Object(m) => m,
        _ => return fail(label, format!("{at} must be a mapping")),
    };
    for (key, value) in map {
        match key.as_str() {
            "context" => t.fields.context = Some(string_field(label, &at, key, value)?),
            "file" => t.fields.file = Some(string_field(label, &at, key, value)?),
            "target" => t.fields.target = Some(string_field(label, &at, key, value)?),
            "output" => t.fields.output = Some(string_field(label, &at, key, value)?),
            "format" => t.fields.format = Some(string_field(label, &at, key, value)?),
            "arg-files" => t.fields.arg_files = Some(string_list(label, &at, key, value)?),
            "tags" => t.fields.tags = Some(string_list(label, &at, key, value)?),
            "inherits" => {
                t.inherits = match value {
                    Value::String(s) => vec![s.clone()],
                    _ => string_list(label, &at, key, value)?,
                }
            }
            "args" => {
                let Value::Object(m) = value else {
                    return fail(
                        label,
                        format!("{at}: args must be a mapping of NAME: value"),
                    );
                };
                let mut args = Vec::new();
                for (k, a) in m {
                    if !is_key(k) {
                        return fail(label, format!("{at}: '{k}' is not a valid ARG name"));
                    }
                    if !is_scalar(a) {
                        return fail(
                            label,
                            format!("{at}: args.{k} must be a string, number or boolean"),
                        );
                    }
                    args.push((k.clone(), js_string_of(a)));
                }
                t.fields.args = Some(args);
            }
            "matrix" => {
                let m = match value {
                    Value::Object(m) if !m.is_empty() => m,
                    _ => {
                        return fail(
                            label,
                            format!("{at}: matrix must be a mapping of name: [values]"),
                        )
                    }
                };
                let mut matrix = Vec::new();
                for (k, list) in m {
                    if !is_key(k) {
                        return fail(
                            label,
                            format!("{at}: '{k}' is not a valid matrix variable name"),
                        );
                    }
                    match list {
                        Value::Array(a) if !a.is_empty() && a.iter().all(is_scalar) => matrix.push((k.clone(), a.iter().map(js_string_of).collect())),
                        _ => return fail(label, format!("{at}: matrix.{k} must be a non-empty list of strings, numbers or booleans")),
                    }
                }
                t.matrix = Some(matrix);
            }
            _ => {
                return fail(
                    label,
                    format!("{at}: unknown key '{key}' (expected {TARGET_KEYS})"),
                )
            }
        }
    }
    Ok(t)
}

/// Check the shape of a parsed bake file.
pub fn parse_bake_definition(value: &Value, label: &str) -> Result<Definition> {
    let Value::Object(top) = value else {
        return fail(
            label,
            "a bake file must be a mapping with targets (and optional groups)",
        );
    };
    for key in top.keys() {
        if key != "targets" && key != "groups" {
            return fail(
                label,
                format!("unknown key '{key}' (expected targets, groups)"),
            );
        }
    }
    let raw_targets = match top.get("targets") {
        Some(Value::Object(m)) if !m.is_empty() => m,
        _ => return fail(label, "targets must be a non-empty mapping of name: target"),
    };
    let mut targets = Vec::new();
    for (name, t) in raw_targets {
        if !is_name(name) {
            return fail(
                label,
                format!("'{name}' is not a valid target name (letters, digits, '.', '_', '-')"),
            );
        }
        targets.push((name.clone(), parse_target(label, name, t)?));
    }
    let mut groups = Vec::new();
    match top.get("groups") {
        None | Some(Value::Null) => {}
        Some(Value::Object(m)) => {
            for (name, list) in m {
                if !is_name(name) {
                    return fail(
                        label,
                        format!(
                            "'{name}' is not a valid group name (letters, digits, '.', '_', '-')"
                        ),
                    );
                }
                if targets.iter().any(|(n, _)| n == name) {
                    return fail(label, format!("'{name}' is both a target and a group"));
                }
                groups.push((
                    name.clone(),
                    string_list(label, &format!("group '{name}'"), "the group", list)?,
                ));
            }
        }
        Some(_) => return fail(label, "groups must be a mapping of name: [targets]"),
    }
    Ok(Definition {
        label: label.to_string(),
        targets,
        groups,
    })
}

fn merge(into: &mut Fields, from: &Fields) {
    macro_rules! take {
        ($($f:ident),*) => { $( if from.$f.is_some() { into.$f = from.$f.clone(); } )* };
    }
    take!(context, file, target, output, format, arg_files, tags);
    if let Some(args) = &from.args {
        let acc = into.args.get_or_insert_with(Vec::new);
        for (k, v) in args {
            match acc.iter_mut().find(|(n, _)| n == k) {
                Some(e) => e.1 = v.clone(),
                None => acc.push((k.clone(), v.clone())),
            }
        }
    }
}

/// A target's fields with its `inherits` applied (parents in order, then its own fields).
fn inherited(def: &Definition, name: &str, stack: &[String]) -> Result<Fields> {
    let t = def.target(name).expect("known target");
    let mut out = Fields::default();
    for parent in &t.inherits {
        if def.target(parent).is_none() {
            return fail(
                &def.label,
                format!("target '{name}' inherits unknown target '{parent}'"),
            );
        }
        if parent == name || stack.contains(parent) {
            let mut chain: Vec<String> = stack.to_vec();
            chain.push(name.to_string());
            chain.push(parent.clone());
            return fail(
                &def.label,
                format!("inherits cycle: {}", chain.join(" -> ")),
            );
        }
        let mut next = stack.to_vec();
        next.push(name.to_string());
        merge(&mut out, &inherited(def, parent, &next)?);
    }
    merge(&mut out, &t.fields); // matrix and inherits are not inherited
    Ok(out)
}

/// Every combination of the matrix values, the first variable varying slowest.
fn combinations(matrix: &[(String, Vec<String>)]) -> Vec<Vec<(String, String)>> {
    let mut out: Vec<Vec<(String, String)>> = vec![Vec::new()];
    for (k, values) in matrix {
        out = out
            .iter()
            .flat_map(|c| {
                values.iter().map(move |v| {
                    let mut n = c.clone();
                    n.push((k.clone(), v.clone()));
                    n
                })
            })
            .collect();
    }
    out
}

/// Replace `${var}` with matrix values; any other `${...}` is an error.
fn interpolate(
    def: &Definition,
    name: &str,
    field: &str,
    s: &str,
    vars: &[(String, String)],
) -> Result<String> {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        let Some(j) = rest[i + 2..].find('}') else {
            break;
        };
        let var = &rest[i + 2..i + 2 + j];
        out.push_str(&rest[..i]);
        match vars.iter().find(|(k, _)| k == var) {
            Some((_, v)) => out.push_str(v),
            None => {
                let names: Vec<&str> = vars.iter().map(|(k, _)| k.as_str()).collect();
                return fail(
                    &def.label,
                    format!(
                        "target '{name}': {field}: '${{{var}}}' is not a matrix variable ({})",
                        names.join(", ")
                    ),
                );
            }
        }
        rest = &rest[i + 2 + j + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn join_to(dir: &str, p: &str) -> String {
    if p.starts_with('/') {
        normalize_posix(p)
    } else {
        join_path(dir, p)
    }
}

/// A matrix value as part of a target name: `/[^A-Za-z0-9._-]/g` -> `_` (per UTF-16 unit, as in JavaScript).
fn name_part(v: &str) -> String {
    v.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c.to_string()
            } else {
                "_".repeat(c.len_utf16())
            }
        })
        .collect()
}

/// Expand and resolve every target of the definition, in file order (matrix targets in place).
fn expand(def: &Definition, bake_dir: &str) -> Result<Vec<(String, Vec<BakeTarget>)>> {
    let mut out = Vec::new();
    let mut seen: Vec<(String, String)> = Vec::new();
    for (base, raw) in &def.targets {
        let fields = inherited(def, base, &[])?;
        let combos = match &raw.matrix {
            Some(m) => combinations(m),
            None => vec![Vec::new()],
        };
        let mut list = Vec::new();
        for combo in combos {
            let mut name = base.clone();
            for (_, v) in &combo {
                name.push('-');
                name.push_str(&name_part(v));
            }
            if let Some((_, prior)) = seen.iter().find(|(n, _)| *n == name) {
                return fail(
                    &def.label,
                    format!("two targets are named '{name}' (from '{prior}' and '{base}')"),
                );
            }
            seen.push((name.clone(), base.clone()));
            let sub = |field: &str, s: &str| -> Result<String> {
                if raw.matrix.is_some() {
                    interpolate(def, &name, field, s, &combo)
                } else {
                    Ok(s.to_string())
                }
            };
            let format = match &fields.format {
                Some(f) => Some(sub("format", f)?),
                None => None,
            };
            let parsed = match &format {
                Some(f) => match ExportFormat::parse(f) {
                    Some(p) => p,
                    None => {
                        return fail(
                            &def.label,
                            format!(
                                "target '{name}': format must be one of {} (or tar.gz), got '{f}'",
                                crate::cli::images::EXPORT_FORMATS
                            ),
                        )
                    }
                },
                None => ExportFormat::Directory,
            };
            let context = join_to(
                bake_dir,
                &sub("context", fields.context.as_deref().unwrap_or("."))?,
            );
            let mut args = Vec::new();
            for (k, v) in fields.args.iter().flatten() {
                args.push((k.clone(), sub(&format!("args.{k}"), v)?));
            }
            let default_out = join_path(
                "dist",
                &format!("{name}{}", &default_out_name(parsed, None)["dist".len()..]),
            );
            let file = match &fields.file {
                Some(f) => join_to(bake_dir, &sub("file", f)?),
                None => join_path(&context, "Sitefile"),
            };
            let mut arg_files = Vec::new();
            for f in fields.arg_files.iter().flatten() {
                arg_files.push(join_to(bake_dir, &sub("arg-files", f)?));
            }
            let target = match &fields.target {
                Some(t) => Some(sub("target", t)?),
                None => None,
            };
            let output = join_to(
                bake_dir,
                &match &fields.output {
                    Some(o) => sub("output", o)?,
                    None => default_out,
                },
            );
            let mut tags = Vec::new();
            for t in fields.tags.iter().flatten() {
                tags.push(sub("tags", t)?);
            }
            list.push(BakeTarget {
                name: name.clone(),
                context,
                file,
                args,
                arg_files,
                target,
                output,
                format,
                tags,
                matrix: raw.matrix.as_ref().map(|_| combo.clone()),
            });
        }
        out.push((base.clone(), list));
    }
    Ok(out)
}

/// Resolve a bake definition into the targets to build (see `resolveBake` in src/bake.ts).
pub fn resolve_bake(def: &Definition, file: &str, selected: &[String]) -> Result<BakePlan> {
    let bake_dir = dirname(file);
    let all = expand(def, &bake_dir)?;
    let mut picked: Vec<BakeTarget> = Vec::new();
    fn add(picked: &mut Vec<BakeTarget>, t: &BakeTarget) {
        if !picked.iter().any(|p| p.name == t.name) {
            picked.push(t.clone());
        }
    }
    fn visit(
        def: &Definition,
        all: &[(String, Vec<BakeTarget>)],
        picked: &mut Vec<BakeTarget>,
        name: &str,
        stack: &[String],
        from_group: Option<&str>,
    ) -> Result<()> {
        if let Some(group) = def.group(name) {
            if stack.iter().any(|s| s == name) {
                let mut chain = stack.to_vec();
                chain.push(name.to_string());
                return fail(
                    &def.label,
                    format!("group '{name}' includes itself ({})", chain.join(" -> ")),
                );
            }
            let mut next = stack.to_vec();
            next.push(name.to_string());
            for member in group {
                visit(def, all, picked, member, &next, Some(name))?;
            }
            return Ok(());
        }
        if let Some((_, list)) = all.iter().find(|(b, _)| b == name) {
            for t in list {
                add(picked, t);
            }
            return Ok(());
        }
        if let Some(t) = all.iter().flat_map(|(_, l)| l).find(|t| t.name == name) {
            add(picked, t);
            return Ok(());
        }
        if let Some(g) = from_group {
            return fail(
                &def.label,
                format!("group '{g}': no target or group named '{name}'"),
            );
        }
        let targets: Vec<&str> = def.targets.iter().map(|(n, _)| n.as_str()).collect();
        let groups: Vec<&str> = def.groups.iter().map(|(n, _)| n.as_str()).collect();
        Err(SitefileError::plain(
            E_USAGE,
            format!(
                "bake: no target or group named '{name}' (targets: {}{})",
                targets.join(", "),
                if groups.is_empty() {
                    String::new()
                } else {
                    format!("; groups: {}", groups.join(", "))
                }
            ),
        ))
    }
    if !selected.is_empty() {
        for n in selected {
            visit(def, &all, &mut picked, n, &[], None)?;
        }
    } else if def.group("default").is_some() {
        visit(def, &all, &mut picked, "default", &[], None)?;
    } else {
        for (_, list) in &all {
            for t in list {
                add(&mut picked, t);
            }
        }
    }
    check_conflicts(&picked)?;
    Ok(BakePlan {
        file: file.to_string(),
        targets: picked,
    })
}

/// `path.resolve`.
fn absolute(p: &str) -> String {
    let s = if p.starts_with('/') {
        normalize_posix(p)
    } else {
        join_path(
            &std::env::current_dir()
                .unwrap_or_default()
                .to_string_lossy(),
            p,
        )
    };
    if s.len() > 1 {
        s.trim_end_matches('/').to_string()
    } else {
        s
    }
}

fn check_conflicts(targets: &[BakeTarget]) -> Result<()> {
    let abs: Vec<String> = targets.iter().map(|t| absolute(&t.output)).collect();
    let under = |a: &str, b: &str| {
        a.starts_with(&if b.ends_with('/') {
            b.to_string()
        } else {
            format!("{b}/")
        })
    };
    for i in 0..targets.len() {
        for j in i + 1..targets.len() {
            let (a, b) = (&targets[i], &targets[j]);
            if abs[i] == abs[j] {
                return Err(SitefileError::plain(
                    E_CONFLICT,
                    format!(
                        "bake targets '{}' and '{}' both write '{}'",
                        a.name, b.name, a.output
                    ),
                ));
            }
            if under(&abs[j], &abs[i]) || under(&abs[i], &abs[j]) {
                return Err(SitefileError::plain(
                    E_CONFLICT,
                    format!(
                        "bake targets '{}' and '{}' write overlapping outputs '{}' and '{}'",
                        a.name, b.name, a.output, b.output
                    ),
                ));
            }
            if let Some(tag) = a.tags.iter().find(|t| b.tags.contains(t)) {
                return Err(SitefileError::plain(
                    E_CONFLICT,
                    format!(
                        "bake targets '{}' and '{}' both tag '{tag}'",
                        a.name, b.name
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Read, check and resolve a bake file.
pub fn bake_plan(file: &str, selected: &[String]) -> Result<BakePlan> {
    resolve_bake(
        &parse_bake_definition(&load_bake_file(file)?, file)?,
        file,
        selected,
    )
}

fn pairs(p: &[(String, String)]) -> Value {
    Value::Object(
        p.iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect::<Map<_, _>>(),
    )
}

/// `--print --json`.
pub fn bake_plan_json(plan: &BakePlan) -> Value {
    let targets: Vec<Value> = plan
        .targets
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "context": t.context,
                "file": t.file,
                "args": pairs(&t.args),
                "argFiles": t.arg_files,
                "target": t.target,
                "output": t.output,
                "format": t.format,
                "tags": t.tags,
                "matrix": t.matrix.as_ref().map(|m| pairs(m)),
            })
        })
        .collect();
    json!({ "file": plan.file, "targets": targets })
}

/// `--print`: one block per target, then a count.
pub fn bake_plan_text(plan: &BakePlan) -> String {
    let line = |label: &str, value: &str| format!("  {label:<10}{value}\n");
    let mut s = String::new();
    for t in &plan.targets {
        s.push_str(&format!("{}\n", t.name));
        s.push_str(&line("context", &t.context));
        s.push_str(&line("file", &t.file));
        if let Some(x) = &t.target {
            s.push_str(&line("target", x));
        }
        s.push_str(&line("output", &t.output));
        if let Some(f) = &t.format {
            s.push_str(&line("format", f));
        }
        for (k, v) in &t.args {
            s.push_str(&line("arg", &format!("{k}={v}")));
        }
        for f in &t.arg_files {
            s.push_str(&line("arg-file", f));
        }
        for tag in &t.tags {
            s.push_str(&line("tag", tag));
        }
        if let Some(m) = &t.matrix {
            s.push_str(&line(
                "matrix",
                &m.iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(" "),
            ));
        }
    }
    let n = plan.targets.len();
    s + &format!(
        "{n} target{} from {}\n",
        if n == 1 { "" } else { "s" },
        plan.file
    )
}

/// The `sitefile build` arguments of one target (before the CLI's pass-through flags).
pub fn build_argv(t: &BakeTarget) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "build".into(),
        t.context.clone(),
        "-f".into(),
        t.file.clone(),
        "-o".into(),
        t.output.clone(),
    ];
    if let Some(x) = &t.target {
        a.extend(["--target".into(), x.clone()]);
    }
    if let Some(f) = &t.format {
        a.extend(["--format".into(), f.clone()]);
    }
    for f in &t.arg_files {
        a.extend(["--build-arg-file".into(), f.clone()]);
    }
    for (k, v) in &t.args {
        a.extend(["--build-arg".into(), format!("{k}={v}")]);
    }
    for tag in &t.tags {
        a.extend(["-t".into(), tag.clone()]);
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(yaml: &str, selected: &[&str]) -> Result<BakePlan> {
        let v = parse_yaml(yaml, "b.yaml")?;
        let def = parse_bake_definition(&v, "b.yaml")?;
        resolve_bake(
            &def,
            "site/b.yaml",
            &selected.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        )
    }

    fn names(p: &BakePlan) -> Vec<&str> {
        p.targets.iter().map(|t| t.name.as_str()).collect()
    }

    #[test]
    fn defaults_and_paths() {
        let p = plan("targets:\n  a:\n  b:\n    context: ./b\n    format: zip\n    output: /tmp/x/../out.zip\n", &[]).unwrap();
        assert_eq!(names(&p), ["a", "b"]);
        let a = &p.targets[0];
        assert_eq!(
            (a.context.as_str(), a.file.as_str(), a.output.as_str()),
            ("site", "site/Sitefile", "site/dist/a")
        );
        let b = &p.targets[1];
        assert_eq!(
            (b.context.as_str(), b.file.as_str(), b.output.as_str()),
            ("site/b", "site/b/Sitefile", "/tmp/out.zip")
        );
        assert_eq!(
            build_argv(a),
            ["build", "site", "-f", "site/Sitefile", "-o", "site/dist/a"]
        );
    }

    #[test]
    fn matrix_inherits_groups() {
        let y = "groups:\n  default: [site]\n  all: [default, other]\ntargets:\n  base:\n    args: {A: 1, B: x}\n    output: dist/${l}\n  site:\n    inherits: base\n    matrix:\n      l: [en, fr]\n      e: [s, p]\n    args: {B: '${l}-${e}'}\n    output: dist/${l}/${e}\n  other:\n";
        let p = plan(y, &[]).unwrap();
        assert_eq!(
            names(&p),
            ["site-en-s", "site-en-p", "site-fr-s", "site-fr-p"]
        );
        let t = &p.targets[1];
        assert_eq!(
            t.args,
            vec![
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "en-p".to_string())
            ]
        );
        assert_eq!(t.output, "site/dist/en/p");
        assert_eq!(
            t.matrix.as_ref().unwrap(),
            &vec![
                ("l".to_string(), "en".to_string()),
                ("e".to_string(), "p".to_string())
            ]
        );
        assert_eq!(
            names(&plan(y, &["all"]).unwrap()),
            ["site-en-s", "site-en-p", "site-fr-s", "site-fr-p", "other"]
        );
        assert_eq!(
            names(&plan(y, &["site-fr-p", "other", "site-fr-p"]).unwrap()),
            ["site-fr-p", "other"]
        );
        let e = plan(y, &["nope"]).unwrap_err();
        assert_eq!(e.code, E_USAGE);
        assert_eq!(e.detail, "bake: no target or group named 'nope' (targets: base, site, other; groups: default, all)");
    }

    #[test]
    fn errors() {
        let err = |y: &str| plan(y, &[]).unwrap_err().detail;
        assert_eq!(
            err("targets:\n  a:\n    inherits: b\n  b:\n    inherits: [a]\n"),
            "b.yaml: inherits cycle: a -> b -> a"
        );
        assert_eq!(
            err("targets:\n  a:\n    inherits: x\n"),
            "b.yaml: target 'a' inherits unknown target 'x'"
        );
        assert_eq!(
            err("targets:\n  a:\n    matrix: {l: [en]}\n    output: ${x}\n"),
            "b.yaml: target 'a-en': output: '${x}' is not a matrix variable (l)"
        );
        assert_eq!(
            err("targets:\n  a:\n    bogus: 1\n"),
            format!("b.yaml: target 'a': unknown key 'bogus' (expected {TARGET_KEYS})")
        );
        assert_eq!(err("targets:\n  a:\n    format: rar\n"), "b.yaml: target 'a': format must be one of directory, zip, tar, tgz, wbn, oci (or tar.gz), got 'rar'");
        assert_eq!(
            err("targets:\n  a:\n    output: out\n  b:\n    output: out/\n"),
            "bake targets 'a' and 'b' both write 'site/out'"
        );
        assert_eq!(
            err("targets:\n  a:\n    output: out\n  b:\n    output: out/b\n"),
            "bake targets 'a' and 'b' write overlapping outputs 'site/out' and 'site/out/b'"
        );
        assert_eq!(
            err("targets:\n  a:\n    tags: [x]\n  b:\n    tags: [y, x]\n"),
            "bake targets 'a' and 'b' both tag 'x'"
        );
        assert_eq!(
            err("groups:\n  a: [a]\ntargets:\n  a:\n"),
            "b.yaml: 'a' is both a target and a group"
        );
        assert_eq!(
            err("groups:\n  default: [g]\n  g: [default]\ntargets:\n  a:\n"),
            "b.yaml: group 'default' includes itself (default -> g -> default)"
        );
        assert_eq!(
            err("targets:\n  a:\n    matrix: {v: ['x y', 'x?y']}\n"),
            "b.yaml: two targets are named 'a-x_y' (from 'a' and 'a')"
        );
        assert_eq!(
            err("targets: {}\n"),
            "b.yaml: targets must be a non-empty mapping of name: target"
        );
    }
}

//! Frontmatter and data schemas (`IMPORT posts FROM ./posts AS markdown SCHEMA ./post.schema.yaml`),
//! a port of `src/schema.ts`: a small, deterministic subset of JSON Schema (`required`, `properties` with
//! `type` string/number/boolean/date/list, `items` and `enum`). docs/LANGUAGE.md has the rules.

use crate::documents::num;
use crate::error::{ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::pattern::{compile_pattern, Pattern};
use crate::site::parse_feed_date;
use crate::values::js_number;
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaType {
    String,
    Number,
    Boolean,
    Date,
    List,
}

const TYPES: [&str; 5] = ["string", "number", "boolean", "date", "list"];
const ITEM_TYPES: [&str; 4] = ["string", "number", "boolean", "date"];

impl SchemaType {
    fn parse(s: &str) -> Option<SchemaType> {
        Some(match s {
            "string" => SchemaType::String,
            "number" => SchemaType::Number,
            "boolean" => SchemaType::Boolean,
            "date" => SchemaType::Date,
            "list" => SchemaType::List,
            _ => return None,
        })
    }
    /// What a value of this type looks like, for messages.
    fn expected(self) -> &'static str {
        match self {
            SchemaType::String => "a string",
            SchemaType::Number => "a number",
            SchemaType::Boolean => "true or false",
            SchemaType::Date => "a date (YYYY-MM-DD or an RFC 3339 date-time)",
            SchemaType::List => "a list",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SchemaRule {
    pub key: String,
    pub ty: Option<SchemaType>,
    pub items: Option<SchemaType>,
    pub allowed: Option<Vec<Value>>,
    /// `pattern`: strings (or a list's items) must match it somewhere (`pattern.rs`).
    pub pattern: Option<Pattern>,
}

/// A compiled schema; `label` names its file in messages.
#[derive(Debug, Clone)]
pub struct Schema {
    pub label: String,
    pub required: Vec<String>,
    pub properties: Vec<SchemaRule>,
}

/// `a, b or c`.
fn or_list(xs: &[&str]) -> String {
    format!("{} or {}", xs[..xs.len() - 1].join(", "), xs[xs.len() - 1])
}

/// Compile a parsed schema file. Errors are E_SCHEMA at `span` (the SCHEMA clause), prefixed with `what`.
pub fn compile_schema(value: &Value, label: &str, what: &str, span: &SourceSpan) -> Result<Schema> {
    let fail =
        |m: String| SitefileError::at(E_SCHEMA, format!("{what}: schema '{label}' {m}"), span);
    let Value::Object(root) = value else {
        return Err(fail(
            "must be a mapping (with required and/or properties)".into(),
        ));
    };
    for k in root.keys() {
        if k != "required" && k != "properties" {
            return Err(fail(format!(
                "has an unknown key '{k}' (expected required or properties)"
            )));
        }
    }
    let mut required: Vec<String> = Vec::new();
    if let Some(r) = root.get("required") {
        let keys: Option<Vec<&str>> = match r {
            Value::Array(a) => a
                .iter()
                .map(|k| k.as_str().filter(|s| !s.is_empty()))
                .collect(),
            _ => None,
        };
        let Some(keys) = keys else {
            return Err(fail("needs 'required' to be a list of key names".into()));
        };
        for k in keys {
            if !required.iter().any(|x| x == k) {
                required.push(k.to_string());
            }
        }
    }
    let mut properties = Vec::new();
    if let Some(p) = root.get("properties") {
        let Value::Object(p) = p else {
            return Err(fail(
                "needs 'properties' to be a mapping of key names to rules".into(),
            ));
        };
        for (key, rule) in p {
            let at = format!("property '{key}':");
            let type_name = |t: &Value, allowed: &[&str], kind: &str| -> Result<SchemaType> {
                match t.as_str() {
                    Some(s) if allowed.contains(&s) => {
                        Ok(SchemaType::parse(s).expect("known type"))
                    }
                    _ => {
                        let shown = t.as_str().map(|s| format!(" '{s}'")).unwrap_or_default();
                        Err(fail(format!(
                            "{at} unknown {kind}{shown} (expected {})",
                            or_list(allowed)
                        )))
                    }
                }
            };
            if let Value::String(_) = rule {
                properties.push(SchemaRule {
                    key: key.clone(),
                    ty: Some(type_name(rule, &TYPES, "type")?),
                    items: None,
                    allowed: None,
                    pattern: None,
                });
                continue;
            }
            let Value::Object(r) = rule else {
                return Err(fail(format!(
                    "{at} expected a type name or a mapping with type, items, enum or pattern"
                )));
            };
            for k in r.keys() {
                if k != "type" && k != "items" && k != "enum" && k != "pattern" {
                    return Err(fail(format!(
                        "{at} unknown key '{k}' (expected type, items, enum or pattern)"
                    )));
                }
            }
            let ty = match r.get("type") {
                Some(t) => Some(type_name(t, &TYPES, "type")?),
                None => None,
            };
            let items = match r.get("items") {
                Some(i) => {
                    if ty != Some(SchemaType::List) {
                        return Err(fail(format!("{at} items needs type list")));
                    }
                    Some(type_name(i, &ITEM_TYPES, "items type")?)
                }
                None => None,
            };
            let allowed = match r.get("enum") {
                Some(e) => match e {
                    Value::Array(a)
                        if !a.is_empty()
                            && a.iter().all(|x| {
                                matches!(x, Value::String(_) | Value::Number(_) | Value::Bool(_))
                            }) =>
                    {
                        Some(a.clone())
                    }
                    _ => {
                        return Err(fail(format!(
                            "{at} enum must be a non-empty list of strings, numbers or booleans"
                        )))
                    }
                },
                None => None,
            };
            let pattern = match r.get("pattern") {
                None => None,
                Some(Value::String(src)) => match compile_pattern(src) {
                    Ok(p) => Some(p),
                    Err(e) => {
                        return Err(fail(format!("{at} pattern '{src}' is invalid {e}")));
                    }
                },
                Some(_) => {
                    return Err(fail(format!(
                        "{at} pattern must be a string (a regular expression)"
                    )))
                }
            };
            properties.push(SchemaRule {
                key: key.clone(),
                ty,
                items,
                allowed,
                pattern,
            });
        }
    }
    Ok(Schema {
        label: label.to_string(),
        required,
        properties,
    })
}

/// `String(v)` for a scalar.
fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => js_number(n.as_f64().unwrap_or(f64::NAN)),
        Value::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

/// How a value is shown in a mismatch message.
fn describe(v: &Value) -> String {
    match v {
        Value::String(s) => format!("string '{s}'"),
        Value::Number(_) => format!("number {}", text(v)),
        Value::Bool(b) => format!("boolean {b}"),
        Value::Array(_) => "a list".into(),
        Value::Object(_) => "an object".into(),
        Value::Null => "null".into(),
    }
}

fn has_type(v: &Value, t: SchemaType, csv: bool) -> bool {
    match t {
        SchemaType::String => v.is_string(),
        SchemaType::Number => v.is_number() || (csv && v.is_string() && num(v).is_some()),
        SchemaType::Boolean => {
            v.is_boolean() || (csv && matches!(v.as_str(), Some("true" | "false")))
        }
        SchemaType::Date => v.as_str().is_some_and(|s| parse_feed_date(s).is_some()),
        SchemaType::List => v.is_array(),
    }
}

/// Why a value fails a `pattern`, or `None` when it matches. Only strings can match.
fn pattern_mismatch(v: &Value, p: &Pattern) -> Option<String> {
    match v.as_str() {
        None => Some(format!(
            "it should be a string matching the pattern '{}'",
            p.source
        )),
        Some(s) if p.matches(s) => None,
        Some(_) => Some(format!("it should match the pattern '{}'", p.source)),
    }
}

fn in_enum(v: &Value, allowed: &[Value], csv: bool) -> bool {
    allowed.iter().any(|e| {
        let same = match (v, e) {
            (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
            _ => v == e,
        };
        same || (csv && v.as_str().is_some_and(|s| s == text(e)))
    })
}

/// Check an import's value against a schema: each element of a list, or a single document. Every element must
/// be an object (E_TYPE otherwise). The first mismatch fails with E_SCHEMA at `span`, naming the document (its
/// `path`, or `#n` for the n-th element), the key, what was expected and what was found.
pub fn check_schema(
    value: &Value,
    schema: &Schema,
    what: &str,
    csv: bool,
    span: &SourceSpan,
) -> Result<()> {
    let docs: Vec<&Value> = match value {
        Value::Array(a) => a.iter().collect(),
        v => vec![v],
    };
    for (i, doc) in docs.into_iter().enumerate() {
        let Value::Object(doc) = doc else {
            return Err(SitefileError::at(
                E_TYPE,
                format!(
                    "{what}: SCHEMA checks documents (objects), but element {} is {}",
                    i + 1,
                    describe(doc)
                ),
                span,
            ));
        };
        check_document(doc, i, schema, what, csv, span)?;
    }
    Ok(())
}

fn check_document(
    doc: &Map<String, Value>,
    i: usize,
    schema: &Schema,
    what: &str,
    csv: bool,
    span: &SourceSpan,
) -> Result<()> {
    let name = match doc.get("path") {
        Some(Value::String(p)) => p.clone(),
        _ => format!("#{}", i + 1),
    };
    let fail = |m: String| {
        Err(SitefileError::at(
            E_SCHEMA,
            format!("{what}: document '{name}' {m}"),
            span,
        ))
    };
    // A CSV cell is always there: an empty one counts as missing.
    let present = |key: &str| {
        doc.get(key)
            .filter(|v| !v.is_null() && !(csv && v.as_str() == Some("")))
    };
    for key in &schema.required {
        if present(key).is_none() {
            return fail(format!(
                "has no '{key}' (required by schema '{}')",
                schema.label
            ));
        }
    }
    let enum_text = |a: &[Value]| a.iter().map(text).collect::<Vec<_>>().join(", ");
    for rule in &schema.properties {
        let Some(v) = present(&rule.key) else {
            continue;
        };
        let key = &rule.key;
        if let Some(t) = rule.ty {
            if !has_type(v, t, csv) {
                return fail(format!(
                    "has '{key}' {}; it should be {}",
                    describe(v),
                    t.expected()
                ));
            }
        }
        if rule.ty == Some(SchemaType::List) {
            for (j, item) in v.as_array().into_iter().flatten().enumerate() {
                if let Some(t) = rule.items {
                    if !has_type(item, t, csv) {
                        return fail(format!(
                            "has '{key}' item {} {}; it should be {}",
                            j + 1,
                            describe(item),
                            t.expected()
                        ));
                    }
                }
                if let Some(a) = &rule.allowed {
                    if !in_enum(item, a, csv) {
                        return fail(format!(
                            "has '{key}' item {} {}; it should be one of {}",
                            j + 1,
                            describe(item),
                            enum_text(a)
                        ));
                    }
                }
                if let Some(m) = rule
                    .pattern
                    .as_ref()
                    .and_then(|p| pattern_mismatch(item, p))
                {
                    return fail(format!(
                        "has '{key}' item {} {}; {m}",
                        j + 1,
                        describe(item)
                    ));
                }
            }
        } else {
            if let Some(a) = &rule.allowed {
                if !in_enum(v, a, csv) {
                    return fail(format!(
                        "has '{key}' {}; it should be one of {}",
                        describe(v),
                        enum_text(a)
                    ));
                }
            }
            if let Some(m) = rule.pattern.as_ref().and_then(|p| pattern_mismatch(v, p)) {
                return fail(format!("has '{key}' {}; {m}", describe(v)));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn span() -> SourceSpan {
        SourceSpan::new("Sitefile", 1, 1)
    }
    fn compile(v: Value) -> Result<Schema> {
        compile_schema(&v, "./s.yaml", "IMPORT p", &span())
    }
    fn check(schema: &Value, docs: Value, csv: bool) -> std::result::Result<(), String> {
        check_schema(
            &docs,
            &compile(schema.clone()).unwrap(),
            "IMPORT p",
            csv,
            &span(),
        )
        .map_err(|e| format!("{}: {}", e.code.as_str(), e.detail))
    }

    #[test]
    fn compiles_and_rejects_bad_schemas() {
        let s = compile(json!({"required": ["title", "title", "date"], "properties": {"date": "date", "tags": {"type": "list", "items": "string"}}})).unwrap();
        assert_eq!(s.required, ["title", "date"]);
        assert_eq!(s.properties.len(), 2);
        let err = |v: Value| compile(v).unwrap_err().detail;
        assert_eq!(
            err(json!([1])),
            "IMPORT p: schema './s.yaml' must be a mapping (with required and/or properties)"
        );
        assert_eq!(
            err(json!({"x": 1})),
            "IMPORT p: schema './s.yaml' has an unknown key 'x' (expected required or properties)"
        );
        assert_eq!(
            err(json!({"required": "a"})),
            "IMPORT p: schema './s.yaml' needs 'required' to be a list of key names"
        );
        assert_eq!(err(json!({"properties": {"a": "str"}})), "IMPORT p: schema './s.yaml' property 'a': unknown type 'str' (expected string, number, boolean, date or list)");
        assert_eq!(err(json!({"properties": {"a": {"type": 3}}})), "IMPORT p: schema './s.yaml' property 'a': unknown type (expected string, number, boolean, date or list)");
        assert_eq!(
            err(json!({"properties": {"a": {"items": "string"}}})),
            "IMPORT p: schema './s.yaml' property 'a': items needs type list"
        );
        assert_eq!(err(json!({"properties": {"a": {"type": "list", "items": "list"}}})), "IMPORT p: schema './s.yaml' property 'a': unknown items type 'list' (expected string, number, boolean or date)");
        assert_eq!(err(json!({"properties": {"a": {"enum": []}}})), "IMPORT p: schema './s.yaml' property 'a': enum must be a non-empty list of strings, numbers or booleans");
        assert_eq!(err(json!({"properties": {"a": {"min": 1}}})), "IMPORT p: schema './s.yaml' property 'a': unknown key 'min' (expected type, items, enum or pattern)");
        assert_eq!(err(json!({"properties": {"a": 1}})), "IMPORT p: schema './s.yaml' property 'a': expected a type name or a mapping with type, items, enum or pattern");
    }

    #[test]
    fn patterns() {
        let s = json!({"properties": {"h": {"pattern": "^[a-z0-9-]+$"}, "tags": {"type": "list", "items": "string", "pattern": "^\\w+$"}, "code": {"enum": ["ab1", "cd"], "pattern": "\\d"}}});
        assert_eq!(
            check(
                &s,
                json!({"h": "hello-world", "tags": ["x", "y_2"], "code": "ab1"}),
                false
            ),
            Ok(())
        );
        assert_eq!(check(&s, json!({"h": "Hello World"}), false), Err("E_SCHEMA: IMPORT p: document '#1' has 'h' string 'Hello World'; it should match the pattern '^[a-z0-9-]+$'".into()));
        assert_eq!(check(&s, json!({"h": 12}), false), Err("E_SCHEMA: IMPORT p: document '#1' has 'h' number 12; it should be a string matching the pattern '^[a-z0-9-]+$'".into()));
        assert_eq!(check(&s, json!({"tags": ["ok", "not ok"]}), false), Err("E_SCHEMA: IMPORT p: document '#1' has 'tags' item 2 string 'not ok'; it should match the pattern '^\\w+$'".into()));
        assert_eq!(check(&s, json!({"code": "cd"}), false), Err("E_SCHEMA: IMPORT p: document '#1' has 'code' string 'cd'; it should match the pattern '\\d'".into()));
        let err = |v: Value| compile(v).unwrap_err().detail;
        assert_eq!(err(json!({"properties": {"a": {"pattern": "[a-"}}})), "IMPORT p: schema './s.yaml' property 'a': pattern '[a-' is invalid at character 1: unterminated character class");
        assert_eq!(err(json!({"properties": {"a": {"pattern": ["x"]}}})), "IMPORT p: schema './s.yaml' property 'a': pattern must be a string (a regular expression)");
    }

    #[test]
    fn checks_documents() {
        let s = json!({"required": ["title"], "properties": {"date": "date", "n": "number", "ok": "boolean", "tags": {"type": "list", "items": "string", "enum": ["a", "b"]}, "status": {"enum": ["draft", 2, true]}}});
        assert_eq!(
            check(
                &s,
                json!([{"title": "x", "date": "2026-01-05", "tags": ["a"], "status": 2}]),
                false
            ),
            Ok(())
        );
        assert_eq!(check(&s, json!([{"path": "/a.md", "title": null}]), false), Err("E_SCHEMA: IMPORT p: document '/a.md' has no 'title' (required by schema './s.yaml')".into()));
        assert_eq!(check(&s, json!([{"title": "x"}, {"title": "y", "date": "2026-02-30"}]), false), Err("E_SCHEMA: IMPORT p: document '#2' has 'date' string '2026-02-30'; it should be a date (YYYY-MM-DD or an RFC 3339 date-time)".into()));
        assert_eq!(check(&s, json!({"title": "x", "tags": ["a", 3]}), false), Err("E_SCHEMA: IMPORT p: document '#1' has 'tags' item 2 number 3; it should be a string".into()));
        assert_eq!(check(&s, json!({"title": "x", "tags": ["c"]}), false), Err("E_SCHEMA: IMPORT p: document '#1' has 'tags' item 1 string 'c'; it should be one of a, b".into()));
        assert_eq!(check(&s, json!({"title": "x", "status": "live"}), false), Err("E_SCHEMA: IMPORT p: document '#1' has 'status' string 'live'; it should be one of draft, 2, true".into()));
        assert_eq!(check(&s, json!({"title": "x", "n": 1.5e21}), false), Ok(()));
        assert_eq!(
            check(&s, json!({"title": "x", "n": "1"}), false),
            Err(
                "E_SCHEMA: IMPORT p: document '#1' has 'n' string '1'; it should be a number"
                    .into()
            )
        );
        // CSV cells are strings: numbers, booleans and enum entries match their text.
        assert_eq!(
            check(
                &s,
                json!([{"title": "x", "n": " 1.5 ", "ok": "true", "status": "2"}]),
                true
            ),
            Ok(())
        );
        assert_eq!(check(&s, json!([{"title": "x", "ok": "yes"}]), true), Err("E_SCHEMA: IMPORT p: document '#1' has 'ok' string 'yes'; it should be true or false".into()));
        assert_eq!(check(&s, json!([{"title": "x", "n": ""}]), true), Ok(()));
        assert_eq!(
            check(&s, json!([{"title": ""}]), true),
            Err(
                "E_SCHEMA: IMPORT p: document '#1' has no 'title' (required by schema './s.yaml')"
                    .into()
            )
        );
        assert_eq!(
            check(&s, json!(["x"]), false),
            Err(
                "E_TYPE: IMPORT p: SCHEMA checks documents (objects), but element 1 is string 'x'"
                    .into()
            )
        );
    }
}

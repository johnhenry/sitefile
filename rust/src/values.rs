//! Typed ARG/LET values and JavaScript-compatible stringification (src/values.ts).
//!
//! Data values (JSON, YAML, CSV, Markdown documents, …) are `serde_json::Value`
//! everywhere, so both implementations see the same shapes.

use crate::error::{err, ErrorCode::*, Result, SourceSpan};
use crate::language::ast::{Operand, TemplatePart, ValueType};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Scalar {
    Str(String),
    Num(f64),
    Bool(bool),
}

impl Scalar {
    /// `String(v)` in JavaScript.
    pub fn to_js_string(&self) -> String {
        match self {
            Scalar::Str(s) => s.clone(),
            Scalar::Num(n) => js_number(*n),
            Scalar::Bool(b) => b.to_string(),
        }
    }
    pub fn to_json(&self) -> Value {
        match self {
            Scalar::Str(s) => Value::String(s.clone()),
            Scalar::Num(n) => js_number_json(*n),
            Scalar::Bool(b) => Value::Bool(*b),
        }
    }
}

/// `String(n)` for a JS number: integers without a fraction, shortest
/// round-trip decimals otherwise, exponent form from 1e21 (as ECMAScript does).
pub fn js_number(n: f64) -> String {
    if n.is_nan() {
        return "NaN".into();
    }
    if n.is_infinite() {
        return if n > 0.0 {
            "Infinity".into()
        } else {
            "-Infinity".into()
        };
    }
    if n == 0.0 {
        return "0".into();
    }
    let abs = n.abs();
    // ECMAScript Number::toString: plain decimal for 1e-6 <= |n| < 1e21.
    if (1e-6..1e21).contains(&abs) {
        if n.fract() == 0.0 {
            if abs < 9_007_199_254_740_992.0 {
                return format!("{}", n as i64);
            }
            // Beyond 2^53 JS prints the shortest round-trip digits, padded with zeros.
            let e = format!("{abs:e}");
            let (mant, exp) = e.split_once('e').unwrap_or((&e, "0"));
            let digits: String = mant.chars().filter(|c| *c != '.').collect();
            let exp: usize = exp.parse().unwrap_or(0);
            let mut out = String::from(if n < 0.0 { "-" } else { "" });
            out.push_str(&digits);
            out.push_str(&"0".repeat((exp + 1).saturating_sub(digits.len())));
            return out;
        }
        return format!("{n}");
    }
    // Exponent form: shortest digits, `e+NN` / `e-NN`.
    let s = format!("{n:e}");
    match s.split_once('e') {
        Some((m, e)) => {
            let e: i32 = e.parse().unwrap_or(0);
            format!("{m}e{}{}", if e >= 0 { "+" } else { "-" }, e.abs())
        }
        None => s,
    }
}

/// `String(v)` for any JSON value a template may substitute.
pub fn js_string_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.as_f64().map(js_number).unwrap_or_else(|| n.to_string()),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".into(),
        Value::Array(_) | Value::Object(_) => v.to_string(),
    }
}

fn is_number_literal(s: &str) -> bool {
    // ^-?\d+(\.\d+)?([eE][+-]?\d+)?$
    let b = s.as_bytes();
    let mut i = 0;
    if b.get(i) == Some(&b'-') {
        i += 1;
    }
    let start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return false;
    }
    if b.get(i) == Some(&b'.') {
        i += 1;
        let s2 = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == s2 {
            return false;
        }
    }
    if matches!(b.get(i), Some(b'e') | Some(b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+') | Some(b'-')) {
            i += 1;
        }
        let s3 = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == s3 {
            return false;
        }
    }
    i == b.len()
}

/// Convert a raw string to a typed scalar, or fail with E_TYPE.
pub fn coerce(raw: &str, ty: ValueType, span: Option<&SourceSpan>, what: &str) -> Result<Scalar> {
    match ty {
        ValueType::String => Ok(Scalar::Str(raw.to_string())),
        ValueType::Number => {
            if !is_number_literal(raw.trim()) {
                return err(
                    E_TYPE,
                    format!("{what} expects a number, got '{raw}'"),
                    span,
                );
            }
            Ok(Scalar::Num(raw.trim().parse::<f64>().unwrap_or(f64::NAN)))
        }
        ValueType::Boolean => match raw {
            "true" => Ok(Scalar::Bool(true)),
            "false" => Ok(Scalar::Bool(false)),
            _ => err(
                E_TYPE,
                format!("{what} expects true or false, got '{raw}'"),
                span,
            ),
        },
    }
}

pub type Env = HashMap<String, Scalar>;

/// Interpolate an instruction operand with ARG/LET bindings.
pub fn evaluate_operand(op: &Operand, env: &Env) -> Result<String> {
    let mut out = String::new();
    for part in &op.parts {
        match part {
            TemplatePart::Text(t) => out.push_str(t),
            TemplatePart::Var { name, span, .. } => match env.get(name) {
                Some(v) => out.push_str(&v.to_js_string()),
                None => {
                    return err(
                        E_UNDEFINED,
                        format!("undefined variable '{name}'"),
                        Some(span),
                    )
                }
            },
        }
    }
    Ok(out)
}

/// A JSON number for an f64, the way JavaScript models it: integral values in
/// the safe range become integers (so they print as `3`, not `3.0`).
pub fn js_number_json(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_991.0 {
        Value::Number(serde_json::Number::from(n as i64))
    } else {
        serde_json::Number::from_f64(n)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    }
}

/// `JSON.stringify(value)`: no whitespace, JS number formatting (`1e+21`,
/// integers without `.0`), keys in insertion order, or sorted (by UTF-16 code
/// units, like `Object.keys(…).sort()`) when `sort_keys` is set, for canonical
/// JSON such as cache keys and OCI documents.
pub fn js_json_stringify(value: &Value, sort_keys: bool) -> String {
    let mut out = String::new();
    write_js_json(&mut out, value, sort_keys, None, 0);
    out
}

/// `JSON.stringify(value, null, 2)`: what the CLIs print for `--json` (numbers
/// and key order as JavaScript has them, e.g. `9007199254740992`, not `…992.0`).
pub fn js_json_pretty(value: &Value) -> String {
    let mut out = String::new();
    write_js_json(&mut out, value, false, Some(2), 0);
    out
}

/// A newline and `depth` levels of indentation (nothing when compact).
fn newline(out: &mut String, indent: Option<usize>, depth: usize) {
    if let Some(n) = indent {
        out.push('\n');
        out.extend(std::iter::repeat_n(' ', n * depth));
    }
}

fn write_js_json(out: &mut String, v: &Value, sort: bool, indent: Option<usize>, depth: usize) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => out.push_str(&i.to_string()),
            (_, Some(u), _) => out.push_str(&js_number(u as f64)),
            (_, _, Some(f)) if f.is_finite() => out.push_str(&js_number(f)),
            _ => out.push_str("null"),
        },
        Value::String(s) => write_js_string(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, indent, depth + 1);
                write_js_json(out, item, sort, indent, depth + 1);
            }
            if !items.is_empty() {
                newline(out, indent, depth);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            let mut keys: Vec<&String> = map.keys().collect();
            if sort {
                // Object.keys(v).sort(): a plain UTF-16 string sort.
                keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            } else {
                // JS property order: array-index keys first, ascending, then the rest in insertion order.
                keys.sort_by_key(|k| array_index(k).map_or((1, 0), |i| (0, i)));
            }
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, indent, depth + 1);
                write_js_string(out, k);
                out.push_str(if indent.is_some() { ": " } else { ":" });
                write_js_json(out, &map[*k], sort, indent, depth + 1);
            }
            if !keys.is_empty() {
                newline(out, indent, depth);
            }
            out.push('}');
        }
    }
}

/// A canonical array index ("0", "9", "10" … "4294967294"), as ECMAScript defines one.
fn array_index(k: &str) -> Option<u32> {
    if k.is_empty() || (k.len() > 1 && k.starts_with('0')) || !k.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    k.parse::<u64>()
        .ok()
        .filter(|&n| n < 4_294_967_295)
        .map(|n| n as u32)
}

/// JSON string escaping exactly as JSON.stringify does (including lone
/// surrogates, which Rust strings can't hold, so they never occur here).
fn write_js_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn js_numbers() {
        assert_eq!(js_number(3.0), "3");
        assert_eq!(js_number(-2.5), "-2.5");
        assert_eq!(js_number(0.1), "0.1");
        assert_eq!(js_number(1e21), "1e+21");
        assert_eq!(js_number(123456789.0), "123456789");
        // Reference values from node: String(n) beyond 2^53.
        assert_eq!(js_number(12345678901234567890.0), "12345678901234567000");
        assert_eq!(js_number(2f64.powi(53)), "9007199254740992");
        assert_eq!(js_number(2f64.powi(60)), "1152921504606847000");
    }
    #[test]
    fn json_stringify_like_js() {
        let v: Value = serde_json::json!({"b": [1, 2.5, 1e21, true, null], "a": "q\"\n\u{1}"});
        assert_eq!(
            js_json_stringify(&v, false),
            r#"{"b":[1,2.5,1e+21,true,null],"a":"q\"\n\u0001"}"#
        );
        assert_eq!(
            js_json_stringify(&v, true),
            r#"{"a":"q\"\n\u0001","b":[1,2.5,1e+21,true,null]}"#
        );
        assert_eq!(js_json_stringify(&js_number_json(3.0), false), "3");
        // Reference values from node: String(n) and JSON.stringify key order.
        for (n, js) in [
            (1.5e-7, "1.5e-7"),
            (1e-6, "0.000001"),
            (1e-7, "1e-7"),
            (123e-20, "1.23e-18"),
            (1e21, "1e+21"),
            (-0.000001, "-0.000001"),
        ] {
            assert_eq!(js_number(n), js, "{n}");
        }
        let o: Value =
            serde_json::from_str(r#"{"b":1,"10":2,"9":3,"a":4,"4294967295":5,"01":6}"#).unwrap();
        assert_eq!(
            js_json_stringify(&o, false),
            r#"{"9":3,"10":2,"b":1,"a":4,"4294967295":5,"01":6}"#
        );
    }
    #[test]
    fn json_pretty_like_js() {
        // Reference: JSON.stringify(v, null, 2) in node.
        let v: Value = serde_json::json!({"a": 9007199254740992.0, "b": [], "c": {}, "d": [1e20, {"e": null}], "f": 1e21});
        assert_eq!(
            js_json_pretty(&v),
            "{\n  \"a\": 9007199254740992,\n  \"b\": [],\n  \"c\": {},\n  \"d\": [\n    100000000000000000000,\n    {\n      \"e\": null\n    }\n  ],\n  \"f\": 1e+21\n}"
        );
        assert_eq!(js_json_pretty(&serde_json::json!([])), "[]");
    }
    #[test]
    fn coercion() {
        assert_eq!(
            coerce("42", ValueType::Number, None, "x").unwrap(),
            Scalar::Num(42.0)
        );
        assert!(coerce("4x", ValueType::Number, None, "x").is_err());
        assert!(coerce("yes", ValueType::Boolean, None, "x").is_err());
    }
}

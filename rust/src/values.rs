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
            Scalar::Num(n) => serde_json::Number::from_f64(*n).map(Value::Number).unwrap_or(Value::Null),
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
        return if n > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if n == 0.0 {
        return "0".into();
    }
    let abs = n.abs();
    if (1e-7..1e21).contains(&abs) {
        if n.fract() == 0.0 {
            return format!("{}", n as i128);
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
                return err(E_TYPE, format!("{what} expects a number, got '{raw}'"), span);
            }
            Ok(Scalar::Num(raw.trim().parse::<f64>().unwrap_or(f64::NAN)))
        }
        ValueType::Boolean => match raw {
            "true" => Ok(Scalar::Bool(true)),
            "false" => Ok(Scalar::Bool(false)),
            _ => err(E_TYPE, format!("{what} expects true or false, got '{raw}'"), span),
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
                None => return err(E_UNDEFINED, format!("undefined variable '{name}'"), Some(span)),
            },
        }
    }
    Ok(out)
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
    }
    #[test]
    fn coercion() {
        assert_eq!(coerce("42", ValueType::Number, None, "x").unwrap(), Scalar::Num(42.0));
        assert!(coerce("4x", ValueType::Number, None, "x").is_err());
        assert!(coerce("yes", ValueType::Boolean, None, "x").is_err());
    }
}

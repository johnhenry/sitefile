//! What a `RENDER … FOR x IN list [BY key] [PAGE n [FIRST <dest>]]` loop renders, a port of `src/pages.ts`:
//! groups (`{key, count, items}`, keys in SORT BY order) and pages (`{[key,] count, items, number, total}`;
//! the executor adds `url`, `prev` and `next`).

use crate::documents::{cmp_values, merge_sort};
use crate::error::{ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::values::js_string_of;
use serde_json::{json, Map, Value};
use std::cmp::Ordering;

/// Group documents by a field; `what` prefixes messages (`FOR tag IN posts BY tags`).
pub fn group_by(items: &[Value], key: &str, what: &str, span: &SourceSpan) -> Result<Vec<Value>> {
    let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let Value::Object(doc) = item else {
            return Err(SitefileError::at(
                E_TYPE,
                format!(
                    "{what}: element {} is not a document (an object with a '{key}' field)",
                    i + 1
                ),
                span,
            ));
        };
        let name = match doc.get("path") {
            Some(Value::String(p)) => p.clone(),
            _ => format!("#{}", i + 1),
        };
        let values: Vec<&Value> = match doc.get(key) {
            None => vec![],
            Some(Value::Array(a)) => a.iter().collect(),
            Some(v) => vec![v],
        };
        let mut seen: Vec<String> = Vec::new();
        for x in values {
            match x {
                Value::Null => continue,
                Value::String(s) if s.is_empty() => continue,
                Value::String(_) | Value::Number(_) | Value::Bool(_) => {}
                _ => {
                    return Err(SitefileError::at(
                        E_TYPE,
                        format!(
                            "{what}: '{key}' of document '{name}' is not text or a list of text"
                        ),
                        span,
                    ))
                }
            }
            let k = js_string_of(x);
            if seen.contains(&k) {
                continue;
            }
            seen.push(k.clone());
            match groups.iter_mut().find(|(g, _)| *g == k) {
                Some((_, list)) => list.push(item.clone()),
                None => groups.push((k, vec![item.clone()])),
            }
        }
    }
    let sorted = merge_sort(
        groups,
        &|a: &(String, Vec<Value>), b: &(String, Vec<Value>)| {
            cmp_values(&Value::String(a.0.clone()), &Value::String(b.0.clone()))
                .unwrap_or(Ordering::Equal)
        },
    );
    Ok(sorted
        .into_iter()
        .map(|(k, items)| json!({ "key": k, "count": items.len(), "items": items }))
        .collect())
}

/// Cut each group (`(key, items)`; no key for the whole list) into pages of `size` items.
pub fn paginate(
    groups: &[(Option<String>, Vec<Value>)],
    size: usize,
) -> Vec<Vec<Map<String, Value>>> {
    groups
        .iter()
        .map(|(key, items)| {
            let total = items.len().div_ceil(size).max(1);
            (0..total)
                .map(|i| {
                    let mut m = Map::new();
                    if let Some(k) = key {
                        m.insert("key".into(), json!(k));
                    }
                    m.insert("count".into(), json!(items.len()));
                    let end = ((i + 1) * size).min(items.len());
                    m.insert(
                        "items".into(),
                        Value::Array(items[(i * size).min(items.len())..end].to_vec()),
                    );
                    m.insert("number".into(), json!(i + 1));
                    m.insert("total".into(), json!(total));
                    m
                })
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_in_sort_order_and_pages() {
        let span = SourceSpan::new("Sitefile", 1, 1);
        let docs = vec![
            json!({"t": ["b", 10, "a"]}),
            json!({"t": 9}),
            json!({"t": ["a", "a", null, ""]}),
            json!({"x": 1}),
        ];
        let g = group_by(&docs, "t", "FOR", &span).unwrap();
        let keys: Vec<(&str, u64)> = g
            .iter()
            .map(|g| (g["key"].as_str().unwrap(), g["count"].as_u64().unwrap()))
            .collect();
        assert_eq!(keys, [("9", 1), ("10", 1), ("a", 2), ("b", 1)]);
        let e = group_by(&[json!({"t": [{"a": 1}]})], "t", "FOR t IN p BY t", &span).unwrap_err();
        assert_eq!(
            e.detail,
            "FOR t IN p BY t: 't' of document '#1' is not text or a list of text"
        );
        let pages = paginate(&[(None, vec![json!(1), json!(2), json!(3)])], 2);
        assert_eq!(
            Value::Array(pages[0].iter().cloned().map(Value::Object).collect()),
            json!([{"count": 3, "items": [1, 2], "number": 1, "total": 2}, {"count": 3, "items": [3], "number": 2, "total": 2}])
        );
        assert_eq!(paginate(&[(None, vec![])], 5)[0].len(), 1);
    }
}

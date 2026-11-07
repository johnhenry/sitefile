//! RFC 4180 CSV parser (port of `src/csv.ts`; no dependencies).
//!
//! - Fields may be double-quoted; inside quotes `""` is a literal quote and
//!   delimiters / newlines are data. Records end at LF or CRLF; a trailing
//!   newline is optional; a leading UTF-8 BOM is stripped.
//! - Completely empty lines are skipped.
//! - A quote inside an unquoted field, text after a closing quote, or an
//!   unterminated quote is E_DATA with line and column (UTF-16 code units).
//! - `header: true` (default): the first record names the columns (non-empty,
//!   unique) and every record must have the same number of fields. Values are
//!   always strings. With `header: false`, records are returned as string
//!   arrays and may have differing lengths.
//! - Rows are ordinary objects in JavaScript key order (integer-like column
//!   names first); a `__proto__` column is plain data.

use crate::error::{ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::render::js;
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CsvOptions {
    pub header: bool,
    /// `,`, `;` or `\t`.
    pub delimiter: u8,
}

impl Default for CsvOptions {
    fn default() -> Self {
        CsvOptions {
            header: true,
            delimiter: b',',
        }
    }
}

struct Row {
    fields: Vec<String>,
    cols: Vec<u32>,
    line: u32,
}

/// Parse CSV. With `header` (default) the result is an array of objects
/// (column name to string); without, an array of string arrays.
pub fn parse_csv(text: &str, file: &str, options: &CsvOptions) -> Result<Value> {
    if !options.header {
        let rows = tokenize(strip_bom(text), file, options.delimiter)?;
        return Ok(Value::Array(
            rows.into_iter()
                .map(|r| Value::Array(r.fields.into_iter().map(Value::String).collect()))
                .collect(),
        ));
    }
    Ok(Value::Array(
        records(text, file, options.delimiter)?
            .into_iter()
            .map(Value::Object)
            .collect(),
    ))
}

/// Parse CSV with a header row into an array with one object per record.
pub fn parse_csv_records(text: &str, file: &str) -> Result<Value> {
    Ok(Value::Array(
        records(text, file, b',')?
            .into_iter()
            .map(Value::Object)
            .collect(),
    ))
}

fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

fn records(text: &str, file: &str, delim: u8) -> Result<Vec<Map<String, Value>>> {
    let rows = tokenize(strip_bom(text), file, delim)?;
    let Some(head) = rows.first() else {
        return Ok(Vec::new());
    };

    let mut seen = std::collections::HashSet::new();
    for (i, name) in head.fields.iter().enumerate() {
        let span = SourceSpan::new(file, head.line, head.cols[i]);
        if name.is_empty() {
            return Err(SitefileError::new(
                E_DATA,
                format!("empty CSV column name in header (column {})", i + 1),
                Some(span),
            ));
        }
        if !seen.insert(name.as_str()) {
            return Err(SitefileError::new(
                E_DATA,
                format!("duplicate CSV column name '{name}'"),
                Some(span),
            ));
        }
    }

    let n = head.fields.len();
    rows[1..]
        .iter()
        .map(|row| {
            if row.fields.len() != n {
                return Err(SitefileError::new(
                    E_DATA,
                    format!(
                        "expected {n} field{} but found {}",
                        if n == 1 { "" } else { "s" },
                        row.fields.len()
                    ),
                    Some(SourceSpan::new(file, row.line, 1)),
                ));
            }
            let map: Map<String, Value> = head
                .fields
                .iter()
                .cloned()
                .zip(row.fields.iter().cloned().map(Value::String))
                .collect();
            Ok(js::object_order(map))
        })
        .collect()
}

fn tokenize(text: &str, file: &str, delim: u8) -> Result<Vec<Row>> {
    let b = text.as_bytes();
    let n = b.len();
    let mut rows: Vec<Row> = Vec::new();
    let mut i = 0usize;
    let mut line = 1u32;
    let mut line_start = 0usize;
    // Columns count UTF-16 code units from the start of the line.
    let col = |index: usize, ls: usize| js::utf16_len(&text[ls..index]) as u32 + 1;
    let fail = |detail: &str, l: u32, index: usize, ls: usize| -> SitefileError {
        SitefileError::new(
            E_DATA,
            detail,
            Some(SourceSpan::new(file, l, col(index, ls))),
        )
    };
    let crlf = |i: usize| b[i] == b'\r' && b.get(i + 1) == Some(&b'\n');

    while i < n {
        // Skip fully empty lines.
        if b[i] == b'\n' {
            i += 1;
            line += 1;
            line_start = i;
            continue;
        }
        if crlf(i) {
            i += 2;
            line += 1;
            line_start = i;
            continue;
        }

        let mut row = Row {
            fields: Vec::new(),
            cols: Vec::new(),
            line,
        };
        loop {
            row.cols.push(col(i, line_start));
            let field;
            if b.get(i) == Some(&b'"') {
                let (q_line, q_index, q_start) = (line, i, line_start);
                i += 1;
                let mut buf: Vec<u8> = Vec::new();
                loop {
                    if i >= n {
                        return Err(fail("unterminated quoted field", q_line, q_index, q_start));
                    }
                    let c = b[i];
                    if c == b'"' {
                        if b.get(i + 1) == Some(&b'"') {
                            buf.push(b'"');
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    if c == b'\n' {
                        line += 1;
                        line_start = i + 1;
                    }
                    buf.push(c);
                    i += 1;
                }
                let at_end = i >= n || b[i] == delim || b[i] == b'\n' || crlf(i);
                if !at_end {
                    return Err(fail(
                        "unexpected character after closing quote",
                        line,
                        i,
                        line_start,
                    ));
                }
                // Only ASCII bytes were split off, so this is valid UTF-8.
                field = String::from_utf8_lossy(&buf).into_owned();
            } else {
                let s = i;
                while i < n {
                    let c = b[i];
                    if c == delim || c == b'\n' || crlf(i) {
                        break;
                    }
                    if c == b'"' {
                        return Err(fail("quote inside an unquoted field", line, i, line_start));
                    }
                    i += 1;
                }
                field = text[s..i].to_string();
            }
            row.fields.push(field);

            if i >= n {
                break;
            }
            if b[i] == delim {
                i += 1;
                continue;
            }
            // Record terminator.
            i += if b[i] == b'\r' { 2 } else { 1 };
            line += 1;
            line_start = i;
            break;
        }
        rows.push(row);
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn csv(t: &str) -> Value {
        parse_csv_records(t, "data.csv").unwrap()
    }
    fn opts(header: bool, delimiter: u8) -> CsvOptions {
        CsvOptions { header, delimiter }
    }
    fn fails(t: &str, line: u32, column: u32, needle: &str) {
        let e = parse_csv_records(t, "data.csv").unwrap_err();
        assert_eq!(e.code, E_DATA);
        assert_eq!(
            e.span,
            Some(SourceSpan::new("data.csv", line, column)),
            "{t:?}: {e}"
        );
        assert!(e.detail.contains(needle), "{}", e.detail);
    }

    #[test]
    fn basic_records_keep_strings() {
        assert_eq!(
            csv("name,age,ok\nAda,36,true\nLin,,false\n"),
            json!([{ "name": "Ada", "age": "36", "ok": "true" }, { "name": "Lin", "age": "", "ok": "false" }])
        );
    }

    #[test]
    fn empty_and_header_only() {
        assert_eq!(csv(""), json!([]));
        assert_eq!(csv("a,b\n"), json!([]));
        assert_eq!(csv("a,b"), json!([]));
        assert_eq!(parse_csv("", "f", &opts(false, b',')).unwrap(), json!([]));
    }

    #[test]
    fn newlines_crlf_blank_lines() {
        let expected = json!([{ "a": "1", "b": "2" }, { "a": "3", "b": "4" }]);
        for t in [
            "a,b\n1,2\n3,4",
            "a,b\n1,2\n3,4\n",
            "a,b\r\n1,2\r\n3,4\r\n",
            "a,b\n1,2\n\n3,4\n\n",
            "a,b\r\n1,2\r\n\r\n3,4",
        ] {
            assert_eq!(csv(t), expected, "{t:?}");
        }
    }

    #[test]
    fn bom_is_stripped() {
        assert_eq!(csv("\u{feff}a,b\n1,2\n"), json!([{ "a": "1", "b": "2" }]));
        assert_eq!(
            parse_csv_records("\u{feff}a\n1\n", "f").unwrap()[0]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["a"]
        );
    }

    #[test]
    fn quoted_fields() {
        assert_eq!(
            csv("a,b\n\"x \"\"y\"\" z\",\"1,2\"\n\"line1\nline2\",\"\"\n"),
            json!([{ "a": "x \"y\" z", "b": "1,2" }, { "a": "line1\nline2", "b": "" }])
        );
        assert_eq!(
            csv("a,b\r\n\"l1\r\nl2\",x\r\n"),
            json!([{ "a": "l1\r\nl2", "b": "x" }])
        );
        assert_eq!(
            csv("\"a b\",\"c,d\"\n1,2\n"),
            json!([{ "a b": "1", "c,d": "2" }])
        );
        assert_eq!(csv("a\n\"\"\"\"\n"), json!([{ "a": "\"" }]));
        assert_eq!(csv("a\n\"é😀\"\n"), json!([{ "a": "é😀" }]));
    }

    #[test]
    fn whitespace_trailing_delimiter_and_delimiters() {
        assert_eq!(csv("a,b\n x , y \n"), json!([{ "a": " x ", "b": " y " }]));
        assert_eq!(csv("a,b\n1,\n"), json!([{ "a": "1", "b": "" }]));
        assert_eq!(csv("a,b\n1,"), json!([{ "a": "1", "b": "" }]));
        assert_eq!(
            parse_csv("a;b\n1,5;\"x;y\"\n", "f", &opts(true, b';')).unwrap(),
            json!([{ "a": "1,5", "b": "x;y" }])
        );
        assert_eq!(
            parse_csv("a\tb\n1\t\"x\ty\"\n", "f", &opts(true, b'\t')).unwrap(),
            json!([{ "a": "1", "b": "x\ty" }])
        );
        assert_eq!(
            parse_csv("a,b\n1,2\n", "f", &opts(true, b';')).unwrap(),
            json!([{ "a,b": "1,2" }])
        );
    }

    #[test]
    fn header_false_returns_arrays() {
        assert_eq!(
            parse_csv("a,b\n1\n2,3,4\n", "f", &opts(false, b',')).unwrap(),
            json!([["a", "b"], ["1"], ["2", "3", "4"]])
        );
        assert_eq!(
            parse_csv("\"x,y\",z\n", "f", &opts(false, b',')).unwrap(),
            json!([["x,y", "z"]])
        );
    }

    #[test]
    fn proto_column_and_key_order() {
        let rows = parse_csv_records("__proto__,b\nx,y\n", "f").unwrap();
        assert_eq!(
            rows[0].as_object().unwrap().keys().collect::<Vec<_>>(),
            ["__proto__", "b"]
        );
        assert_eq!(rows[0]["__proto__"], json!("x"));
        let rows = parse_csv_records("b,2,a,1\nw,x,y,z\n", "f").unwrap();
        assert_eq!(
            rows[0].as_object().unwrap().keys().collect::<Vec<_>>(),
            ["1", "2", "b", "a"]
        );
    }

    #[test]
    fn differential_fixtures() {
        crate::render::fixtures::check("csv.json", |t| {
            parse_csv(t, "f.csv", &CsvOptions::default())
        });
        crate::render::fixtures::check("csv-raw.json", |t| {
            parse_csv(t, "f.csv", &opts(false, b','))
        });
    }

    #[test]
    fn errors() {
        fails("a,,c\n1,2,3\n", 1, 3, "empty CSV column name");
        fails("a,b,a\n1,2,3\n", 1, 5, "duplicate CSV column name 'a'");
        fails("\n\na,b,b\n", 3, 5, "duplicate");
        fails("a,b\n1,2\n3\n", 3, 1, "expected 2 fields but found 1");
        fails("a,b\n1,2,3\n", 2, 1, "expected 2 fields but found 3");
        fails("a\n\"x\ny\"\n1,2\n", 4, 1, "expected 1 field but found 2");
        fails("a,b\n1,\"open\nmore\n", 2, 3, "unterminated");
        fails("\"never closed", 1, 1, "unterminated");
        fails("a,b\n1,x\"y\"\n", 2, 4, "quote inside an unquoted field");
        fails("a\"b,c\n", 1, 2, "quote inside");
        fails("a,b\n\"x\"y,2\n", 2, 4, "after closing quote");
        fails("a\n\"x\ny\"z\n", 3, 3, "after closing quote");
        // UTF-16 columns
        fails("😀,b\"\n", 1, 5, "quote inside");
    }
}

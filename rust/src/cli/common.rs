//! Shared pieces of the store/server commands (port of `src/cli/common.ts`).

use super::args::{flag_s, parse, OptSpec, Parsed};
use crate::error::{ErrorCode, Result, SitefileError};
use std::io::Write;

pub struct Command {
    pub name: &'static str,
    /// One line for the command list.
    pub summary: &'static str,
    pub usage: &'static str,
    pub description: &'static str,
    /// Help lines for the options, already formatted.
    pub flags: &'static [&'static str],
    pub options: &'static [OptSpec],
    pub run: fn(&Parsed) -> Result<i32>,
}

pub fn out(s: &str) {
    let mut o = std::io::stdout().lock();
    let _ = o.write_all(s.as_bytes());
    let _ = o.flush();
}

pub fn err(s: &str) {
    let mut o = std::io::stderr().lock();
    let _ = o.write_all(s.as_bytes());
    let _ = o.flush();
}

pub fn command_help(c: &Command) -> String {
    let flags = if c.flags.is_empty() {
        String::new()
    } else {
        format!(
            "\nOptions:\n{}\n",
            c.flags
                .iter()
                .map(|f| format!("  {f}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };
    format!("Usage: sitefile {}\n\n{}\n{flags}", c.usage, c.description)
}

pub fn usage_error(message: &str) -> i32 {
    err(&format!(
        "error: {message}\nRun 'sitefile --help' for usage.\n"
    ));
    2
}

/// Print a SitefileError the way the CLI does and return the exit code (1, or 2 for E_USAGE).
pub fn report(e: &SitefileError) -> i32 {
    if e.code == ErrorCode::E_USAGE {
        return usage_error(&e.detail);
    }
    err(&format!("error[{}]: {}\n", e.code, e.message()));
    1
}

/// [`report`], and with `--json` (`json`) also the error as JSON on stdout
/// (`{ "error": { code, message, span, needs } }`; not for usage errors).
pub fn report_json(e: &SitefileError, json: bool) -> i32 {
    if json && e.code != ErrorCode::E_USAGE {
        out(&format!("{}\n", pretty_json(&e.to_json())));
    }
    report(e)
}

/// Parse `args` for `command` and run it, mapping SitefileError to the CLI exit codes.
pub fn run_command(command: &Command, args: &[String]) -> i32 {
    let mut options: Vec<OptSpec> = command.options.to_vec();
    options.push(flag_s("help", 'h'));
    let parsed = match parse(args, &options) {
        Ok(p) => p,
        Err(m) => return usage_error(&format!("{}: {m}", command.name)),
    };
    if parsed.flag("help") {
        out(&command_help(command));
        return 0;
    }
    match (command.run)(&parsed) {
        Ok(code) => code,
        // With --json, the failure is also machine-readable on stdout (code, span, needs).
        Err(e) => report_json(&e, parsed.flag("json")),
    }
}

pub fn usage<T>(message: impl Into<String>) -> Result<T> {
    Err(SitefileError::plain(ErrorCode::E_USAGE, message))
}

pub fn require_args(positionals: &[String], min: usize, max: usize, what: &str) -> Result<()> {
    if positionals.len() < min {
        return usage(format!("expected {what}"));
    }
    if positionals.len() > max {
        return usage(format!(
            "unexpected arguments: {}",
            positionals[max..].join(" ")
        ));
    }
    Ok(())
}

/// Left-aligned columns separated by two spaces, like `docker images`.
pub fn table(rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let len = |s: &str| s.encode_utf16().count();
    let mut widths: Vec<usize> = Vec::new();
    for r in rows {
        for (i, c) in r.iter().enumerate() {
            if widths.len() <= i {
                widths.push(0);
            }
            widths[i] = widths[i].max(len(c));
        }
    }
    let lines: Vec<String> = rows
        .iter()
        .map(|r| {
            let cells: Vec<String> = r
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    if i == r.len() - 1 {
                        c.clone()
                    } else {
                        format!("{c}{}", " ".repeat(widths[i].saturating_sub(len(c))))
                    }
                })
                .collect();
            cells.join("  ").trim_end().to_string()
        })
        .collect();
    lines.join("\n") + "\n"
}

/// `Number.prototype.toPrecision(3)` for 1 <= v < 1000.
fn to_precision3(v: f64) -> String {
    let s = format!("{v:.2e}");
    let (mant, exp) = s.split_once('e').unwrap_or((&s, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let digits: String = mant.chars().filter(|c| c.is_ascii_digit()).collect();
    if exp < 0 {
        return format!("0.{}{digits}", "0".repeat((-exp - 1) as usize));
    }
    let int_len = exp as usize + 1;
    if int_len >= digits.len() {
        format!("{digits}{}", "0".repeat(int_len - digits.len()))
    } else {
        format!("{}.{}", &digits[..int_len], &digits[int_len..])
    }
}

/// Drop trailing zeros only after a decimal point ("1.50" -> "1.5", "100" stays).
fn strip_zeros(s: &str) -> String {
    if !s.contains('.') {
        return s.to_string();
    }
    let t = s.trim_end_matches('0');
    if t.len() == s.len() {
        return s.to_string();
    }
    t.strip_suffix('.').unwrap_or(t).to_string()
}

/// 1.2kB, 34.5MB (decimal units, as Docker prints them).
pub fn human_size(bytes: u64) -> String {
    if bytes < 1000 {
        return format!("{bytes}B");
    }
    let units = ["kB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i: usize = 0;
    v /= 1000.0;
    while v >= 1000.0 && i < units.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    let n = if v >= 100.0 {
        format!("{}", v.round())
    } else {
        strip_zeros(&to_precision3(v))
    };
    format!("{n}{}", units[i])
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `Date.parse` for ISO timestamps (`YYYY-MM-DDTHH:MM:SS[.sss]Z`), in ms.
pub fn parse_iso(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || !s.ends_with('Z')
    {
        return None;
    }
    let num = |r: std::ops::Range<usize>| {
        s.get(r)
            .filter(|t| t.bytes().all(|c| c.is_ascii_digit()))
            .and_then(|t| t.parse::<i64>().ok())
    };
    let (y, mo, d, h, mi, se) = (
        num(0..4)?,
        num(5..7)?,
        num(8..10)?,
        num(11..13)?,
        num(14..16)?,
        num(17..19)?,
    );
    let mut ms = 0;
    let rest = &s[19..s.len() - 1];
    if !rest.is_empty() {
        let frac = rest.strip_prefix('.')?;
        if frac.is_empty() || !frac.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let f3: String = format!("{frac:0<3}").chars().take(3).collect();
        ms = f3.parse().ok()?;
    }
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 24 || mi > 59 || se > 59 {
        return None;
    }
    Some((days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se) * 1000 + ms)
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn relative_time(iso: &str, now: i64) -> String {
    let Some(t) = parse_iso(iso) else {
        return "unknown".into();
    };
    // Math.round: halves round up.
    let s = (((now - t) as f64 / 1000.0) + 0.5).floor().max(0.0) as i64;
    if s < 5 {
        return "Less than a second ago".into();
    }
    let units: [(i64, &str); 7] = [
        (31_536_000, "year"),
        (2_592_000, "month"),
        (604_800, "week"),
        (86_400, "day"),
        (3600, "hour"),
        (60, "minute"),
        (1, "second"),
    ];
    for (n, name) in units {
        if s >= n {
            let k = s / n;
            return format!("{k} {name}{} ago", if k == 1 { "" } else { "s" });
        }
    }
    "Less than a second ago".into()
}

/// `Number(raw)` (JavaScript string-to-number conversion).
pub fn js_to_number(raw: &str) -> f64 {
    let t = crate::render::js::trim_str(raw);
    if t.is_empty() {
        return 0.0;
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(d) = t.strip_prefix(prefix) {
            if d.is_empty() || !d.chars().all(|c| c.is_digit(radix)) {
                return f64::NAN;
            }
            return d.chars().fold(0.0, |acc, c| {
                acc * radix as f64 + c.to_digit(radix).unwrap_or(0) as f64
            });
        }
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    if !t
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))
    {
        return f64::NAN;
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

/// `Number.isSafeInteger(n)`.
pub fn is_safe_integer(n: f64) -> bool {
    n.is_finite() && n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_991.0
}

/// `JSON.stringify(value, null, 2)`.
pub fn pretty_json(v: &serde_json::Value) -> String {
    crate::values::js_json_pretty(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_like_typescript() {
        assert_eq!(human_size(999), "999B");
        assert_eq!(human_size(1000), "1kB");
        assert_eq!(human_size(1234), "1.23kB");
        assert_eq!(human_size(12_345), "12.3kB");
        assert_eq!(human_size(123_456), "123kB");
        assert_eq!(human_size(1_500_000), "1.5MB");
        assert_eq!(human_size(20_000), "20kB");
        assert_eq!(human_size(99_995), "100kB");
        assert_eq!(human_size(0), "0B");
        assert_eq!(human_size(1023), "1.02kB");
        assert_eq!(human_size(1500), "1.5kB");
        assert_eq!(human_size(100_000), "100kB");
        assert_eq!(human_size(999_999), "1000kB");
    }

    #[test]
    fn times() {
        let t = parse_iso("2024-01-01T00:00:00.000Z").unwrap();
        assert_eq!(t, 1_704_067_200_000);
        assert_eq!(
            relative_time("2024-01-01T00:00:00.000Z", t + 3_000),
            "Less than a second ago"
        );
        assert_eq!(
            relative_time("2024-01-01T00:00:00.000Z", t + 61_000),
            "1 minute ago"
        );
        assert_eq!(
            relative_time("2024-01-01T00:00:00.000Z", t + 3 * 86_400_000),
            "3 days ago"
        );
        assert_eq!(relative_time("garbage", t), "unknown");
    }

    #[test]
    fn numbers() {
        assert_eq!(js_to_number(" 12 "), 12.0);
        assert_eq!(js_to_number("0x10"), 16.0);
        assert_eq!(js_to_number("1e3"), 1000.0);
        assert!(js_to_number("inf").is_nan());
        assert!(js_to_number("12px").is_nan());
        assert_eq!(js_to_number(""), 0.0);
    }

    #[test]
    fn tables() {
        let t = table(&[
            vec!["A".into(), "BB".into()],
            vec!["CCC".into(), "D".into()],
        ]);
        assert_eq!(t, "A    BB\nCCC  D\n");
    }
}

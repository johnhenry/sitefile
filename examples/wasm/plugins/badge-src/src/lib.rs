//! A Sitefile WebAssembly plugin (ABI v1): renders a shields-style SVG badge.
//!
//! Options: `label` (left text), `value` (right text), `color` (#hex or a
//! named color for the right side). Without `value`, the badge shows the
//! reading time of `item.content` (a Markdown document from `FOR post IN
//! posts`) or of the input text, at `wpm` words per minute (default 220).
//!
//! The module imports nothing: it can't touch files, the network or the
//! clock, so the same request always gives the same badge.

mod json;

use json::Json;
use std::alloc::{alloc, Layout};

/// ABI: reserve `len` bytes for the request.
#[no_mangle]
pub extern "C" fn sitefile_alloc(len: i32) -> i32 {
    let layout = Layout::from_size_align(len.max(1) as usize, 1).unwrap();
    unsafe { alloc(layout) as i32 }
}

#[no_mangle]
pub extern "C" fn sitefile_api_version() -> i32 {
    1
}

/// ABI: read the request JSON, return `(ptr << 32) | len` of the result JSON.
#[no_mangle]
pub extern "C" fn sitefile_transform(ptr: i32, len: i32) -> i64 {
    let request = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let out = match run(request) {
        Ok(svg) => format!("{{\"text\":{}}}", json::quote(&svg)),
        Err(e) => format!("{{\"error\":{}}}", json::quote(&e)),
    };
    // Leak the result: the instance is thrown away after this call.
    let bytes = out.into_bytes().into_boxed_slice();
    let len = bytes.len() as i64;
    let ptr = Box::into_raw(bytes) as *mut u8 as i64;
    (ptr << 32) | len
}

fn run(request: &[u8]) -> Result<String, String> {
    let text = std::str::from_utf8(request).map_err(|_| "request is not UTF-8".to_string())?;
    let req = json::parse(text)?;
    let opts = req.get("options");
    let opt = |k: &str| opts.and_then(|o| o.get(k)).and_then(Json::as_str);

    let label = opt("label").unwrap_or("reading time");
    let color = color(opt("color").unwrap_or("#0f766e"))?;
    let value = match opt("value") {
        Some(v) => v.to_string(),
        None => {
            let wpm: u64 = match opt("wpm") {
                Some(w) => w.parse().map_err(|_| format!("option \"wpm\" must be a whole number, got \"{w}\""))?,
                None => 220,
            };
            if wpm == 0 {
                return Err("option \"wpm\" must be at least 1".into());
            }
            let source = req
                .get("item")
                .and_then(|i| i.get("content"))
                .and_then(Json::as_str)
                .or_else(|| req.get("input").and_then(|i| i.get("text")).and_then(Json::as_str))
                .ok_or("no value: pass OPTIONS value=…, a text source, or FOR over Markdown documents")?;
            let words = count_words(&strip_tags(source));
            let minutes = (words + wpm - 1) / wpm;
            format!("{} min · {} words", minutes.max(1), words)
        }
    };
    Ok(badge(label, &value, &color))
}

/// Only `#rgb`, `#rrggbb` or a lowercase CSS color name; anything else is refused.
fn color(c: &str) -> Result<String, String> {
    let hex = c.strip_prefix('#').map(|h| (h.len() == 3 || h.len() == 6) && h.chars().all(|x| x.is_ascii_hexdigit()));
    let named = c.len() <= 20 && c.chars().all(|x| x.is_ascii_lowercase());
    if hex == Some(true) || (hex.is_none() && named) {
        Ok(c.to_string())
    } else {
        Err(format!("option \"color\" must be #rgb, #rrggbb or a color name, got \"{c}\""))
    }
}

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

fn count_words(text: &str) -> u64 {
    text.split_whitespace().filter(|w| w.chars().any(char::is_alphanumeric)).count() as u64
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// Approximate Verdana 11px advance widths, enough to size the badge halves.
fn text_width(s: &str) -> u32 {
    s.chars()
        .map(|c| match c {
            'i' | 'j' | 'l' | '\'' | '|' | '.' | ',' | ':' | ';' => 3,
            'f' | 't' | 'r' | ' ' | '(' | ')' | '-' => 4,
            'm' | 'w' | 'M' | 'W' => 10,
            c if c.is_ascii_uppercase() => 8,
            '·' => 4,
            _ => 7,
        })
        .sum()
}

fn badge(label: &str, value: &str, color: &str) -> String {
    let lw = text_width(label) + 12;
    let vw = text_width(value) + 12;
    let w = lw + vw;
    let (l, v) = (escape_xml(label), escape_xml(value));
    format!(
        concat!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"20\" role=\"img\" aria-label=\"{l}: {v}\">",
            "<title>{l}: {v}</title>",
            "<linearGradient id=\"s\" x2=\"0\" y2=\"100%\"><stop offset=\"0\" stop-color=\"#bbb\" stop-opacity=\".1\"/><stop offset=\"1\" stop-opacity=\".1\"/></linearGradient>",
            "<clipPath id=\"r\"><rect width=\"{w}\" height=\"20\" rx=\"3\" fill=\"#fff\"/></clipPath>",
            "<g clip-path=\"url(#r)\"><rect width=\"{lw}\" height=\"20\" fill=\"#555\"/><rect x=\"{lw}\" width=\"{vw}\" height=\"20\" fill=\"{color}\"/><rect width=\"{w}\" height=\"20\" fill=\"url(#s)\"/></g>",
            "<g fill=\"#fff\" text-anchor=\"middle\" font-family=\"Verdana,Geneva,DejaVu Sans,sans-serif\" font-size=\"11\">",
            "<text x=\"{lx}\" y=\"14\">{l}</text><text x=\"{vx}\" y=\"14\">{v}</text></g></svg>\n"
        ),
        w = w,
        lw = lw,
        vw = vw,
        l = l,
        v = v,
        color = color,
        lx = lw / 2,
        vx = lw + vw / 2,
    )
}

//! WARC/1.1 export and import (port of `src/warc.ts`, issue #206; the format is described there
//! and in docs/LANGUAGE.md).

use crate::deflate::gzip;
use crate::error::SourceSpan;
use crate::error::{ErrorCode::*, Result, SitefileError};
use crate::export::oci::iso_of;
use crate::export::wbn::{mime_type_for, validate_base_url};
use crate::language::ast::OciCompression;
use crate::links::{encode_path, percent_decode};
use crate::resources::types::ArchiveLimits;
use crate::server::{headers_for, parse_header_rules};
use crate::tree::{make_file, Node, Tree};
use sha2::{Digest, Sha256};
use std::io::Read;

const CRLF: &str = "\r\n";

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A deterministic `urn:uuid:` (UUID version 8) from the hash of `seed`.
fn record_id(seed: &str) -> String {
    let h = sha256_hex(seed.as_bytes());
    let variant = (u8::from_str_radix(&h[16..17], 16).unwrap_or(0) & 3) | 8;
    format!(
        "<urn:uuid:{}-{}-8{}-{:x}{}-{}>",
        &h[0..8],
        &h[8..12],
        &h[13..16],
        variant,
        &h[17..20],
        &h[20..32]
    )
}

fn record(fields: &[(&str, String)], block: &[u8]) -> Vec<u8> {
    let mut head = String::from("WARC/1.1\r\n");
    for (k, v) in fields {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str(&format!("Content-Length: {}\r\n\r\n", block.len()));
    let mut out = head.into_bytes();
    out.extend_from_slice(block);
    out.extend_from_slice(b"\r\n\r\n");
    out
}

/// `EXPORT … AS warc`: the tree as WARC/1.1 records.
pub fn write_warc(
    tree: &Tree,
    base_url: Option<&str>,
    epoch: Option<u64>,
    compression: Option<OciCompression>,
) -> Result<Vec<u8>> {
    let Some(raw) = base_url.filter(|b| !b.is_empty()) else {
        return Err(SitefileError::plain(
            E_VALIDATION,
            "warc export needs a base URL: use 'EXPORT <path> AS warc BASE https://example.com/' or --base-url",
        ));
    };
    let base = validate_base_url(Some(raw))?;
    let date = iso_of(epoch);
    let rules = parse_header_rules(&String::from_utf8_lossy(
        tree.get("/_headers").map(|n| &n.bytes[..]).unwrap_or(&[]),
    ));
    let mut records: Vec<Vec<u8>> = Vec::new();
    let info_id = record_id(&format!("warcinfo\0{base}"));
    records.push(record(
        &[
            ("WARC-Type", "warcinfo".into()),
            ("WARC-Record-ID", info_id.clone()),
            ("WARC-Date", date.clone()),
            ("Content-Type", "application/warc-fields".into()),
        ],
        format!("software: sitefile{CRLF}format: WARC File Format 1.1{CRLF}").as_bytes(),
    ));
    let response = |uri: &str, path: &str, node: &Node| -> Vec<u8> {
        let headers = headers_for(&rules, path);
        let ty = headers
            .iter()
            .find(|(k, _)| k == "content-type")
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| mime_type_for(path).to_string());
        let mut lines = vec![
            "HTTP/1.1 200 OK".to_string(),
            format!("Content-Type: {ty}"),
            format!("Content-Length: {}", node.bytes.len()),
        ];
        for (k, v) in &headers {
            if k != "content-type" {
                lines.push(format!("{k}: {v}"));
            }
        }
        let mut block = (lines.join(CRLF) + CRLF + CRLF).into_bytes();
        block.extend_from_slice(&node.bytes);
        let block_digest = format!("sha256:{}", sha256_hex(&block));
        record(
            &[
                ("WARC-Type", "response".into()),
                (
                    "WARC-Record-ID",
                    record_id(&format!("response\0{uri}\0{block_digest}")),
                ),
                ("WARC-Warcinfo-ID", info_id.clone()),
                ("WARC-Date", date.clone()),
                ("WARC-Target-URI", uri.to_string()),
                (
                    "WARC-Payload-Digest",
                    format!("sha256:{}", sha256_hex(&node.bytes)),
                ),
                ("WARC-Block-Digest", block_digest.clone()),
                ("Content-Type", "application/http;msgtype=response".into()),
            ],
            &block,
        )
    };
    let mut entries: Vec<_> = tree.entries().collect();
    entries.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
    for (p, n) in entries {
        if p == "/_headers" || p == "/_redirects" {
            continue;
        }
        records.push(response(&format!("{base}{}", encode_path(&p[1..])), p, n));
        if p == "/index.html" || p.ends_with("/index.html") {
            let dir = &p[..p.len() - "index.html".len()];
            records.push(response(&format!("{base}{}", encode_path(&dir[1..])), p, n));
        }
    }
    Ok(if compression == Some(OciCompression::Gzip) {
        records.iter().flat_map(|r| gzip(r)).collect()
    } else {
        records.concat()
    })
}

/// True when `bytes` start a WARC file, plain or gzipped.
pub fn looks_like_warc(bytes: &[u8]) -> bool {
    if bytes.starts_with(b"WARC/") {
        return true;
    }
    if !bytes.starts_with(&[0x1f, 0x8b]) {
        return false;
    }
    let mut head = [0u8; 5];
    let mut d = flate2::read::GzDecoder::new(&bytes[..bytes.len().min(4096)]);
    d.read_exact(&mut head).is_ok() && &head == b"WARC/"
}

fn inflate(
    bytes: &[u8],
    origin: &str,
    limits: &ArchiveLimits,
    span: Option<&SourceSpan>,
) -> Result<Vec<u8>> {
    let cap = limits
        .max_total_bytes
        .saturating_add(limits.max_entries.saturating_mul(4096))
        .saturating_add(65536);
    let mut out = Vec::new();
    flate2::read::MultiGzDecoder::new(bytes)
        .take(cap.saturating_add(1))
        .read_to_end(&mut out)
        .map_err(|_| {
            SitefileError::plain(E_ARCHIVE, format!("{origin}: corrupt gzip in WARC"))
                .with_span(span)
        })?;
    if out.len() as u64 > cap {
        return Err(SitefileError::plain(
            E_ARCHIVE_LIMIT,
            format!("{origin}: WARC expands beyond {cap} bytes"),
        )
        .with_span(span));
    }
    if out.len() > 1024 * 1024
        && out.len() as u64 > (bytes.len() as u64).saturating_mul(limits.max_ratio)
    {
        return Err(SitefileError::plain(
            E_ARCHIVE_LIMIT,
            format!(
                "{origin}: WARC compression ratio exceeds {}",
                limits.max_ratio
            ),
        )
        .with_span(span));
    }
    Ok(out)
}

fn index_of(b: &[u8], pat: &[u8], from: usize) -> Option<usize> {
    if from > b.len() {
        return None;
    }
    b[from..]
        .windows(pat.len())
        .position(|w| w == pat)
        .map(|i| i + from)
}

fn latin1(b: &[u8]) -> String {
    b.iter().map(|&c| c as char).collect()
}

fn field<'a>(lines: &'a [String], name: &str) -> Option<&'a str> {
    lines.iter().find_map(|l| {
        let i = l.find(':').filter(|i| *i > 0)?;
        (l[..i].trim().to_ascii_lowercase() == name).then(|| l[i + 1..].trim())
    })
}

fn split_url(uri: &str) -> Option<(String, String)> {
    let lower = uri.to_ascii_lowercase();
    let rest = if lower.starts_with("http://") {
        &uri[7..]
    } else if lower.starts_with("https://") {
        &uri[8..]
    } else {
        return None;
    };
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = rest[..host_end].to_lowercase();
    let after = &rest[host_end..];
    let path_end = after.find(['?', '#']).unwrap_or(after.len());
    let path = &after[..path_end];
    Some((
        host,
        if path.is_empty() {
            "/".into()
        } else {
            path.into()
        },
    ))
}

fn is_ok_status(line: &str) -> bool {
    let b = line.as_bytes();
    if !line.starts_with("HTTP/") || b.len() < 6 || !b[5].is_ascii_digit() {
        return false;
    }
    let mut i = 6;
    if b.get(i) == Some(&b'.') && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
        i += 2;
    }
    line[i..].starts_with(" 200") && (line.len() == i + 4 || b[i + 4] == b' ')
}

/// The files of a WARC (plain or gzipped).
pub fn read_warc(
    input: &[u8],
    origin: &str,
    limits: &ArchiveLimits,
    span: Option<&SourceSpan>,
) -> Result<Tree> {
    let owned;
    let bytes: &[u8] = if input.starts_with(&[0x1f, 0x8b]) {
        owned = inflate(input, origin, limits, span)?;
        &owned
    } else {
        input
    };
    let bad =
        |why: String| SitefileError::plain(E_ARCHIVE, format!("{origin}: {why}")).with_span(span);
    let mut files: Vec<(String, Node)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut host: Option<String> = None;
    let mut total: u64 = 0;
    let mut at = 0;
    while at < bytes.len() {
        while at < bytes.len() && (bytes[at] == b'\r' || bytes[at] == b'\n') {
            at += 1;
        }
        if at >= bytes.len() {
            break;
        }
        let head_end = index_of(bytes, b"\r\n\r\n", at)
            .ok_or_else(|| bad("truncated WARC record header".into()))?;
        let lines: Vec<String> = latin1(&bytes[at..head_end])
            .split("\r\n")
            .map(str::to_string)
            .collect();
        if lines[0] != "WARC/1.0" && lines[0] != "WARC/1.1" {
            return Err(bad(format!("not a WARC record at byte {at}")));
        }
        let fields = &lines[1..];
        let len_text = field(fields, "content-length").unwrap_or("");
        if len_text.is_empty()
            || len_text.len() > 15
            || !len_text.bytes().all(|c| c.is_ascii_digit())
        {
            return Err(bad("WARC record without a valid Content-Length".into()));
        }
        let start = head_end + 4;
        let end = start + len_text.parse::<usize>().unwrap_or(usize::MAX / 2);
        if end > bytes.len() {
            return Err(bad("truncated WARC record".into()));
        }
        at = end;
        if field(fields, "warc-type") != Some("response")
            || !field(fields, "content-type")
                .unwrap_or("")
                .to_ascii_lowercase()
                .starts_with("application/http")
        {
            continue;
        }
        let uri = field(fields, "warc-target-uri").unwrap_or("");
        let Some((h, raw_path)) = split_url(uri) else {
            continue;
        };
        let block = &bytes[start..end];
        let Some(http_end) = index_of(block, b"\r\n\r\n", 0) else {
            continue;
        };
        let status = latin1(&block[..http_end.min(64)]);
        if !is_ok_status(status.split("\r\n").next().unwrap_or("")) {
            continue;
        }
        if host.is_none() {
            host = Some(h.clone());
        }
        if host.as_deref() != Some(h.as_str()) {
            continue;
        }
        let mut segs: Vec<String> = raw_path.split('/').skip(1).map(percent_decode).collect();
        if segs.last().is_some_and(String::is_empty) {
            *segs.last_mut().expect("non-empty") = "index.html".into();
        }
        for s in &segs {
            if s.is_empty() || s == "." || s == ".." || s.contains(['\0', '\\', '/']) {
                return Err(SitefileError::plain(
                    E_ARCHIVE_UNSAFE,
                    format!("{origin}: unsafe path in WARC record for {uri}"),
                )
                .with_span(span));
            }
        }
        let p = format!("/{}", segs.join("/"));
        if !seen.insert(p.clone()) {
            continue;
        }
        let body = &block[http_end + 4..];
        total += body.len() as u64;
        if files.len() as u64 + 1 > limits.max_entries {
            return Err(SitefileError::plain(
                E_ARCHIVE_LIMIT,
                format!("{origin}: more than {} entries", limits.max_entries),
            )
            .with_span(span));
        }
        if total > limits.max_total_bytes {
            return Err(SitefileError::plain(
                E_ARCHIVE_LIMIT,
                format!(
                    "{origin}: WARC expands beyond {} bytes",
                    limits.max_total_bytes
                ),
            )
            .with_span(span));
        }
        files.push((p, make_file(body.to_vec(), origin, 0o644)));
    }
    Tree::from_files(files).map_err(|e| {
        SitefileError::plain(E_ARCHIVE_UNSAFE, format!("{origin}: {}", e.message())).with_span(span)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_ids() {
        let t = Tree::from_files(vec![
            (
                "/index.html".to_string(),
                make_file(b"<p>home</p>".to_vec(), "t", 0o644),
            ),
            (
                "/_headers".to_string(),
                make_file(b"/*\n  X-A: 1\n".to_vec(), "t", 0o644),
            ),
        ])
        .unwrap();
        let w = write_warc(&t, Some("https://example.org/"), None, None).unwrap();
        let text = String::from_utf8(w.clone()).unwrap();
        assert!(text.contains("WARC-Target-URI: https://example.org/\r\n"));
        assert!(text.contains("\r\nx-a: 1\r\n\r\n<p>home</p>"));
        assert!(text.contains("<urn:uuid:32a50fc6-5352-8fca-bc5c-8d0ed98d1aa1>"));
        let gz = write_warc(
            &t,
            Some("https://example.org/"),
            None,
            Some(OciCompression::Gzip),
        )
        .unwrap();
        assert!(looks_like_warc(&w) && looks_like_warc(&gz));
        let limits = crate::resources::types::DEFAULT_ARCHIVE_LIMITS;
        for b in [&w, &gz] {
            let back = read_warc(b, "x", &limits, None).unwrap();
            assert_eq!(
                back.entries().map(|(p, _)| p.as_str()).collect::<Vec<_>>(),
                ["/index.html"]
            );
        }
        assert!(
            is_ok_status("HTTP/1.1 200 OK")
                && is_ok_status("HTTP/2 200")
                && !is_ok_status("HTTP/1.1 2000")
        );
    }
}

//! Compression Dictionary Transport (port of `src/dictionary.ts`, issue #215; experimental,
//! `build --experimental-dictionary`): `.dcz` deltas against the previous build and the
//! `Use-As-Dictionary` headers, and the `serve` side that answers `Available-Dictionary`.

use crate::error::{ErrorCode::E_CONFLICT, Result, SitefileError};
use crate::tree::{make_file, ConflictPolicy, Node, Tree};
use crate::zstd::dcz_encode;
use base64::Engine;

const KINDS: [&str; 6] = [".js", ".mjs", ".css", ".json", ".wasm", ".html"];
const MAX_WINDOW: usize = 8 * 1024 * 1024;

pub fn is_dictionary_kind(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    KINDS.iter().any(|k| lower.ends_with(k))
}

fn plain_path(p: &str) -> bool {
    p.bytes()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'/' | b'.' | b'_' | b'~' | b'-'))
}

/// The exported tree with `.dcz` deltas against `previous` and the `Use-As-Dictionary` headers,
/// and the paths that got a delta.
pub fn add_dictionary_deltas(tree: &Tree, previous: &Tree) -> Result<(Tree, Vec<String>)> {
    let mut entries: Vec<(&String, &Node)> = tree.entries().collect();
    entries.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
    let mut added: Vec<(String, Node)> = Vec::new();
    let mut deltas = Vec::new();
    let mut header = String::new();
    for (p, n) in entries {
        if !is_dictionary_kind(p) {
            continue;
        }
        if plain_path(p) {
            header.push_str(&format!("{p}\n  Use-As-Dictionary: match=\"{p}\"\n"));
        }
        let Some(old) = previous.get(p) else { continue };
        if old.digest == n.digest || old.bytes.len() + n.bytes.len() > MAX_WINDOW {
            continue;
        }
        let sidecar = format!("{p}.dcz");
        if tree.get(&sidecar).is_some() {
            return Err(SitefileError::plain(
                E_CONFLICT,
                format!("--experimental-dictionary: '{sidecar}' already exists in the output"),
            ));
        }
        let delta = dcz_encode(&n.bytes, &old.bytes);
        if delta.len() >= n.bytes.len() {
            continue;
        }
        added.push((sidecar, make_file(delta, "dictionary delta", 0o644)));
        deltas.push(p.clone());
    }
    if !header.is_empty() {
        let before = tree
            .get("/_headers")
            .map(|n| String::from_utf8_lossy(&n.bytes).into_owned())
            .unwrap_or_default();
        let sep = if before.is_empty() || before.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        let text = format!("{before}{sep}# Compression Dictionary Transport (sitefile build --experimental-dictionary)\n{header}");
        added.push((
            "/_headers".to_string(),
            make_file(text.into_bytes(), "dictionary headers", 0o644),
        ));
    }
    Ok((
        tree.with_files(added, ConflictPolicy::Overwrite, None)?,
        deltas,
    ))
}

fn sf_binary(v: Option<&str>) -> Option<Vec<u8>> {
    let t = v?.trim_matches(|c: char| c.is_whitespace());
    let inner = t.strip_prefix(':')?.strip_suffix(':')?;
    // `[A-Za-z0-9+/]*={0,2}`, decoded leniently as Node's Buffer does (padding optional).
    let body = inner.trim_end_matches('=');
    if inner.len() - body.len() > 2
        || !body
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'+' || c == b'/')
    {
        return None;
    }
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent)
            .with_decode_allow_trailing_bits(true),
    );
    engine.decode(body).ok()
}

/// True when `Accept-Encoding` lists `dcz` with a non-zero quality.
pub fn accepts_dcz(accept_encoding: Option<&str>) -> bool {
    let Some(v) = accept_encoding else {
        return false;
    };
    v.split(',').any(|part| {
        let mut it = part.split(';').map(str::trim);
        if !it.next().unwrap_or("").eq_ignore_ascii_case("dcz") {
            return false;
        }
        match it.find(|p| p.len() >= 2 && p[..2].eq_ignore_ascii_case("q=")) {
            None => true,
            Some(q) => q[2..].parse::<f64>().map(|x| x > 0.0).unwrap_or(false),
        }
    })
}

/// The `dcz` response for `path` (the bytes of `<path>.dcz`) when the request offers its dictionary.
pub fn delta_for<'a>(
    tree: &'a Tree,
    path: &str,
    available_dictionary: Option<&str>,
    accept_encoding: Option<&str>,
) -> Option<&'a [u8]> {
    if !accepts_dcz(accept_encoding) {
        return None;
    }
    let hash = sf_binary(available_dictionary)?;
    if hash.len() != 32 {
        return None;
    }
    let sidecar = tree.get(&format!("{path}.dcz"))?;
    if sidecar.bytes.len() < 40 || sidecar.bytes[8..40] != hash[..] {
        return None;
    }
    Some(&sidecar.bytes)
}

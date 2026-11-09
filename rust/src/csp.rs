//! CSP (port of `src/csp.ts`; docs/LANGUAGE.md, "Content Security Policy"): a policy per exported page
//! with the hashes of its inline scripts and styles. Headers and pages match the TypeScript.

use crate::links::{inline_blocks, reference_kind};
use crate::site::{csp_meta, csp_policy, page_patterns};
use crate::tree::{make_file, Node, Tree};
use base64::Engine;
use sha2::{Digest, Sha256};

/// `sha256-<base64>` of a text's UTF-8 bytes, as CSP hash sources name it.
pub fn csp_hash(text: &str) -> String {
    format!(
        "sha256-{}",
        base64::engine::general_purpose::STANDARD.encode(Sha256::digest(text.as_bytes()))
    )
}

/// What [`csp_pages`] found: header rules (pattern, policy), pages with a `<meta>`, pages without `<head>`.
#[derive(Default)]
pub struct CspResult {
    pub rules: Vec<(String, String)>,
    pub pages: Vec<(String, Node)>,
    pub headless: Vec<String>,
}

/// See `cspPages` in the TypeScript module.
pub fn csp_pages(
    tree: &Tree,
    export_path: &str,
    base: &str,
    meta: bool,
    origin: &str,
) -> CspResult {
    let mut out = CspResult::default();
    for (p, node) in tree.entries() {
        if reference_kind(p) != Some("html") {
            continue;
        }
        let url = if export_path == "/" {
            Some(p.as_str())
        } else {
            p.strip_prefix(export_path).filter(|r| r.starts_with('/'))
        };
        let Some(url) = url else {
            continue;
        };
        let Ok(text) = std::str::from_utf8(&node.bytes) else {
            continue;
        };
        let blocks = inline_blocks(text);
        let scripts: Vec<String> = blocks.scripts.iter().map(|s| csp_hash(s)).collect();
        let styles: Vec<String> = blocks.styles.iter().map(|s| csp_hash(s)).collect();
        let policy = csp_policy(base, &scripts, &styles);
        if !meta {
            for pattern in page_patterns(url) {
                if !pattern.chars().any(|c| (c as u32) <= 0x20 || c == '\u{7f}') {
                    out.rules.push((pattern, policy.clone()));
                }
            }
        } else if let Some(at) = blocks.head_end {
            let page = format!("{}{}{}", &text[..at], csp_meta(&policy), &text[at..]);
            out.pages
                .push((p.clone(), make_file(page.into_bytes(), origin, node.mode)));
        } else {
            out.headless.push(p.clone());
        }
    }
    out
}

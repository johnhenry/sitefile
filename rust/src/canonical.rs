//! CANONICAL (port of `src/canonical.ts`; docs/LANGUAGE.md, "Site files"): a `<link rel="canonical">`
//! in every page that has none. Pages and messages match the TypeScript implementation.

use crate::error::{Result, SourceSpan};
use crate::links::{canonical_slot, reference_kind, CanonicalSlot};
use crate::site::{page_url, xml_escape};
use crate::tree::{make_file, ConflictPolicy, Node, Tree};

pub struct CanonicalOp<'a> {
    pub base: &'a str,
    pub pretty: bool,
    pub root: &'a str,
    pub span: &'a SourceSpan,
}

/// A page's canonical URL: `<base><url path>`, `…/index.html` as its directory, and with `pretty` no `.html`.
pub fn canonical_url(base: &str, url: &str, pretty: bool) -> String {
    let u = if pretty && url.ends_with(".html") && !url.ends_with("/index.html") {
        &url[..url.len() - ".html".len()]
    } else {
        url
    };
    page_url(base, u)
}

/// See `canonical` in the TypeScript module.
pub fn canonical(tree: Tree, op: &CanonicalOp, origin: &str, warn: &dyn Fn(&str)) -> Result<Tree> {
    let mut files: Vec<(String, Node)> = Vec::new();
    let mut headless: Vec<&str> = Vec::new();
    for (p, node) in tree.entries() {
        if reference_kind(p) != Some("html") {
            continue;
        }
        let url = if op.root == "/" {
            Some(p.as_str())
        } else {
            p.strip_prefix(op.root).filter(|r| r.starts_with('/'))
        };
        let Some(url) = url.filter(|u| *u != "/404.html") else {
            continue;
        };
        let Ok(text) = std::str::from_utf8(&node.bytes) else {
            continue;
        };
        match canonical_slot(text) {
            CanonicalSlot::Has => {}
            CanonicalSlot::NoHead => headless.push(p),
            CanonicalSlot::Before(at) => {
                let link = format!(
                    "<link rel=\"canonical\" href=\"{}\">",
                    xml_escape(&canonical_url(op.base, url, op.pretty))
                );
                let out = format!("{}{link}{}", &text[..at], &text[at..]);
                files.push((p.clone(), make_file(out.into_bytes(), origin, node.mode)));
            }
        }
    }
    if !headless.is_empty() {
        let count = if headless.len() == 1 {
            "1 page has".to_string()
        } else {
            format!("{} pages have", headless.len())
        };
        warn(&format!(
            "{origin}: {count} no </head>, so no canonical link was added: {}",
            headless.join(", ")
        ));
    }
    if files.is_empty() {
        return Ok(tree);
    }
    tree.with_files(files, ConflictPolicy::Overwrite, Some(op.span))
}

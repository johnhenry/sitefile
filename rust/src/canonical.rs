//! CANONICAL (port of `src/canonical.ts`; docs/LANGUAGE.md, "Site files"): a `<link rel="canonical">`
//! in every page that has none. Pages and messages match the TypeScript implementation.

use crate::error::{Result, SourceSpan};
use crate::hreflang::{alternates, Languages};
use crate::links::{head_links, reference_kind};
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
pub fn canonical(
    tree: Tree,
    op: &CanonicalOp,
    origin: &str,
    warn: &dyn Fn(&str),
    langs: Option<&Languages>,
) -> Result<Tree> {
    let mut pages: Vec<(&String, &str, &Node)> = Vec::new();
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
        pages.push((p, url, node));
    }
    let alts = match langs {
        Some(l) => {
            let pairs: Vec<(String, String)> = pages
                .iter()
                .map(|(p, u, _)| ((*p).clone(), u.to_string()))
                .collect();
            Some(alternates(&pairs, l, "CANONICAL", op.span)?)
        }
        None => None,
    };
    let mut files: Vec<(String, Node)> = Vec::new();
    let mut headless: Vec<&str> = Vec::new();
    for (p, url, node) in pages {
        let Ok(text) = std::str::from_utf8(&node.bytes) else {
            continue;
        };
        let head = head_links(text);
        let alt = alts.as_ref().and_then(|a| a.get(p.as_str()));
        let add_alternates = alt.is_some() && !head.hreflang;
        if head.canonical && !add_alternates {
            continue;
        }
        let Some(at) = head.at else {
            headless.push(p);
            continue;
        };
        let mut links = if head.canonical {
            String::new()
        } else {
            format!(
                "<link rel=\"canonical\" href=\"{}\">",
                xml_escape(&canonical_url(op.base, url, op.pretty))
            )
        };
        if add_alternates {
            for (lang, u) in alt.into_iter().flatten() {
                links.push_str(&format!(
                    "<link rel=\"alternate\" hreflang=\"{}\" href=\"{}\">",
                    xml_escape(lang),
                    xml_escape(&canonical_url(op.base, u, op.pretty))
                ));
            }
        }
        let out = format!("{}{links}{}", &text[..at], &text[at..]);
        files.push((p.clone(), make_file(out.into_bytes(), origin, node.mode)));
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

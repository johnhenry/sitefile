//! CHECKLINKS (port of `src/checklinks.ts`; docs/LANGUAGE.md, "Link checking"): find the references in a
//! stage's HTML and CSS files that point at nothing. Same links, order and messages as TypeScript.

use crate::error::{ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::links::{resolve_reference, scan_file, Target};
use crate::resources::https::fetch_remote;
use crate::resources::types::FetchPolicy;
use crate::tree::Tree;
use std::collections::HashMap;

pub struct CheckLinksOp<'a> {
    /// Report broken links as warnings instead of failing.
    pub warn: bool,
    /// Also request http(s) URLs (under the network policy).
    pub external: bool,
    /// The EXPORT path: absolute references resolve below it.
    pub root: &'a str,
    /// REDIRECT sources: a link to one of them is not broken.
    pub redirects: &'a [String],
    pub span: &'a SourceSpan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokenLink {
    pub file: String,
    pub line: u32,
    pub url: String,
    pub reason: String,
}

/// The most broken links an error lists (CHECKLINKS --warn prints every one).
pub const MAX_LISTED: usize = 20;

/// Whether a tree path is served the way `sitefile serve` and static hosts serve it.
pub fn is_served(tree: &Tree, path: &str, dir: bool) -> bool {
    let index = if path == "/" {
        "/index.html".to_string()
    } else {
        format!("{path}/index.html")
    };
    if dir {
        return tree.is_file(&index);
    }
    tree.is_file(path)
        || (path != "/" && tree.is_file(&format!("{path}.html")))
        || (tree.is_directory(path) && tree.is_file(&index))
}

fn url_path(root: &str, path: &str) -> Option<String> {
    if root == "/" {
        return Some(path.to_string());
    }
    if path == root {
        return Some("/".into());
    }
    path.strip_prefix(&format!("{root}/"))
        .map(|rest| format!("/{rest}"))
}

/// Whether a REDIRECT source covers a URL path: equal, or a `/*` prefix of it.
pub fn redirected(sources: &[String], url: &str) -> bool {
    sources.iter().any(|from| {
        from == url
            || from
                .strip_suffix('*')
                .is_some_and(|prefix| from.ends_with("/*") && url.starts_with(prefix))
    })
}

fn external_reason(e: &SitefileError, url: &str, policy: &FetchPolicy) -> Option<String> {
    match e.code {
        E_TOO_LARGE => None,
        E_FETCH_STATUS => {
            let status: u32 = e
                .detail
                .rsplit_once("HTTP ")
                .and_then(|(_, s)| s.parse().ok())
                .unwrap_or(0);
            (status >= 400 || status == 0).then(|| format!("HTTP {status}"))
        }
        E_FETCH_TIMEOUT => Some(format!("no answer within {} ms", policy.timeout_ms)),
        E_PROTOCOL => Some(
            if url
                .get(..5)
                .is_some_and(|s| s.eq_ignore_ascii_case("http:"))
                && !policy.allow_http
            {
                "plain http is checked only with --allow-http"
            } else {
                "not allowed by the network policy"
            }
            .into(),
        ),
        E_SSRF_BLOCKED => {
            Some("a non-public address (checked only with --allow-private-network)".into())
        }
        E_HOST_BLOCKED => Some("refused by the allow-hosts/deny-hosts rules".into()),
        E_REDIRECT => Some("too many or invalid redirects".into()),
        _ => Some("could not connect".into()),
    }
}

/// Every broken link in the HTML and CSS files of `tree` (see `brokenLinks` in the TypeScript module).
pub fn broken_links(tree: &Tree, op: &CheckLinksOp, policy: &FetchPolicy) -> Vec<BrokenLink> {
    let mut out = Vec::new();
    let mut external: HashMap<String, Option<String>> = HashMap::new();
    for (file, node) in tree.entries() {
        let Some((_, refs)) = scan_file(file, &node.bytes) else {
            continue;
        };
        for r in &refs {
            let reason = match resolve_reference(&r.url, file, op.root) {
                Target::None => continue,
                Target::External(u) => {
                    if !op.external {
                        continue;
                    }
                    let url = if u.starts_with("//") {
                        format!("https:{u}")
                    } else {
                        u
                    };
                    if !external.contains_key(&url) {
                        let limited = FetchPolicy {
                            max_bytes: 0,
                            ..policy.clone()
                        };
                        let reason = match fetch_remote(&url, &limited, Some(op.span)) {
                            Ok(_) => None,
                            Err(e) => external_reason(&e, &url, policy),
                        };
                        external.insert(url.clone(), reason);
                    }
                    external[&url].clone()
                }
                Target::Internal { path, dir } => {
                    if is_served(tree, &path, dir) {
                        None
                    } else {
                        let display = if dir && path != "/" {
                            format!("{path}/")
                        } else {
                            path.clone()
                        };
                        let covered = url_path(op.root, &path).is_some_and(|u| {
                            let u = if dir && u != "/" { format!("{u}/") } else { u };
                            redirected(op.redirects, &u)
                        });
                        (!covered).then(|| format!("not found: {display}"))
                    }
                }
            };
            if let Some(reason) = reason {
                out.push(BrokenLink {
                    file: file.clone(),
                    line: r.line,
                    url: r.url.clone(),
                    reason,
                });
            }
        }
    }
    out
}

/// One broken link as messages show it.
pub fn broken_link_text(b: &BrokenLink) -> String {
    format!("{}:{}: {} ({})", b.file, b.line, b.url, b.reason)
}

/// Report the broken links: a warning each with `--warn`, otherwise one E_BROKEN_LINK error listing them.
pub fn report_broken_links(
    broken: &[BrokenLink],
    op: &CheckLinksOp,
    origin: &str,
    warn: &dyn Fn(&str),
) -> Result<()> {
    if broken.is_empty() {
        return Ok(());
    }
    if op.warn {
        for b in broken {
            warn(&format!("{origin}: broken link {}", broken_link_text(b)));
        }
        return Ok(());
    }
    let mut detail = format!(
        "CHECKLINKS: {} broken link{}:",
        broken.len(),
        if broken.len() == 1 { "" } else { "s" }
    );
    for b in broken.iter().take(MAX_LISTED) {
        detail.push_str("\n  ");
        detail.push_str(&broken_link_text(b));
    }
    if broken.len() > MAX_LISTED {
        detail.push_str(&format!(
            "\n  … and {} more (CHECKLINKS --warn lists them all)",
            broken.len() - MAX_LISTED
        ));
    }
    Err(SitefileError::at(E_BROKEN_LINK, detail, op.span))
}

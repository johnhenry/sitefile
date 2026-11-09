//! `EXPORT … AS epub` (port of `src/epub.ts`, issue #209; the layout is described there).

use super::archive_writers::{write_zip_first, WriterOptions};
use super::oci::iso_of;
use super::wbn::mime_type_for;
use crate::error::{ErrorCode::*, Result, SitefileError};
use crate::links::encode_path;
use crate::tree::{make_file, Node, Tree};

fn is_page(p: &str) -> bool {
    let l = p.to_ascii_lowercase();
    l.ends_with(".xhtml") || l.ends_with(".html") || l.ends_with(".htm") || l.ends_with(".xhtm")
}

fn media_type(p: &str) -> String {
    if is_page(p) {
        return "application/xhtml+xml".into();
    }
    mime_type_for(p)
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

/// `&` not starting an entity becomes `&amp;`; `<`, `>` and `"` are escaped.
fn escape_xml(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::new();
    for (i, c) in s.char_indices() {
        match c {
            '&' => {
                let rest = &b[i + 1..];
                let entity = {
                    let (body, ok) = if rest.first() == Some(&b'#') {
                        if rest.get(1) == Some(&b'x') {
                            let n = rest[2..]
                                .iter()
                                .take_while(|c| c.is_ascii_hexdigit())
                                .count();
                            (2 + n, n > 0)
                        } else {
                            let n = rest[1..].iter().take_while(|c| c.is_ascii_digit()).count();
                            (1 + n, n > 0)
                        }
                    } else {
                        let n = rest.iter().take_while(|c| c.is_ascii_alphabetic()).count();
                        (n, n > 0)
                    };
                    ok && rest.get(body) == Some(&b';')
                };
                out.push_str(if entity { "&" } else { "&amp;" });
            }
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

fn find_ci(hay: &str, needle: &str, from: usize) -> Option<usize> {
    let h = hay.as_bytes();
    let n = needle.as_bytes();
    (from..=h.len().saturating_sub(n.len()))
        .find(|&i| h.len() >= i + n.len() && h[i..i + n.len()].eq_ignore_ascii_case(n))
}

/// `<title(?:\s[^>]*)?>([^<]*)</title>` (ASCII case-insensitive), whitespace collapsed.
fn title_of(bytes: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(bytes);
    let mut from = 0;
    while let Some(i) = find_ci(&s, "<title", from) {
        from = i + 1;
        let rest = &s[i + 6..];
        let open_end = match rest.chars().next() {
            Some('>') => Some(0),
            Some(c) if c.is_whitespace() => rest.find('>'),
            _ => None,
        };
        let Some(e) = open_end else { continue };
        let body_start = i + 6 + e + 1;
        let body_len = s[body_start..].find('<').unwrap_or(s.len() - body_start);
        let after = body_start + body_len;
        if !s[after..]
            .get(..8)
            .is_some_and(|t| t.eq_ignore_ascii_case("</title>"))
        {
            continue;
        }
        let t = s[body_start..after]
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        return (!t.is_empty()).then_some(t);
    }
    None
}

/// `<html\b[^>]*?\s(?:xml:)?lang\s*=\s*"([A-Za-z0-9-]+)"` (ASCII case-insensitive).
fn lang_of(bytes: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(bytes);
    let mut from = 0;
    while let Some(i) = find_ci(&s, "<html", from) {
        from = i + 1;
        let after = &s[i + 5..];
        if after
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
        {
            continue;
        }
        let tag_end = after.find('>').unwrap_or(after.len());
        let tag = &after[..tag_end];
        for (j, c) in tag.char_indices() {
            if !c.is_whitespace() {
                continue;
            }
            let rest = &tag[j + c.len_utf8()..];
            let name_len = if rest
                .get(..8)
                .is_some_and(|t| t.eq_ignore_ascii_case("xml:lang"))
            {
                8
            } else if rest
                .get(..4)
                .is_some_and(|t| t.eq_ignore_ascii_case("lang"))
            {
                4
            } else {
                continue;
            };
            let v = rest[name_len..].trim_start();
            let Some(v) = v.strip_prefix('=') else {
                continue;
            };
            let Some(v) = v.trim_start().strip_prefix('"') else {
                continue;
            };
            let n = v
                .bytes()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == b'-')
                .count();
            if n > 0 && v.as_bytes().get(n) == Some(&b'"') {
                return Some(v[..n].to_string());
            }
        }
        // `[^>]*?` cannot cross `>`: the regex tries the next `<html`.
    }
    None
}

fn uuid_of(digest: &str) -> String {
    let h = &digest["sha256:".len()..];
    let variant = (u8::from_str_radix(&h[16..17], 16).unwrap_or(0) & 3) | 8;
    format!(
        "urn:uuid:{}-{}-8{}-{:x}{}-{}",
        &h[0..8],
        &h[8..12],
        &h[13..16],
        variant,
        &h[17..20],
        &h[20..32]
    )
}

/// The files of the EPUB container.
pub fn epub_tree(tree: &Tree, epoch: Option<u64>) -> Result<Tree> {
    let mut files: Vec<(&String, &Node)> = tree
        .entries()
        .filter(|(p, _)| *p != "/_redirects" && *p != "/_headers")
        .collect();
    files.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
    if tree.get("/package.opf").is_some() {
        return Err(SitefileError::plain(
            E_CONFLICT,
            "epub export: '/package.opf' is written by the export; rename the site's file",
        ));
    }
    let mut pages: Vec<(&String, &Node)> = files
        .iter()
        .filter(|(p, _)| is_page(p) && p.as_str() != "/nav.xhtml")
        .cloned()
        .collect();
    if let Some(front) = pages
        .iter()
        .position(|(p, _)| p.as_str() == "/index.xhtml" || p.as_str() == "/index.html")
    {
        let f = pages.remove(front);
        pages.insert(0, f);
    }
    if pages.is_empty() {
        return Err(SitefileError::plain(
            E_EXPORT,
            "epub export: the tree has no .xhtml or .html page to read",
        ));
    }
    let first = &pages[0].1.bytes;
    let title = escape_xml(&title_of(first).unwrap_or_else(|| "Untitled".into()));
    let lang = lang_of(first).unwrap_or_else(|| "en".into());
    let id_of = |p: &str| -> String {
        let i = files.iter().position(|(q, _)| q.as_str() == p).unwrap_or(0);
        format!("f{}", i + 1)
    };
    let href = |p: &str| encode_path(&p[1..]);
    let site_nav = tree.get("/nav.xhtml").is_some();
    let mut items = String::new();
    if !site_nav {
        items.push_str("    <item id=\"nav\" href=\"nav.xhtml\" media-type=\"application/xhtml+xml\" properties=\"nav\"/>\n");
    }
    for (p, _) in &files {
        items.push_str(&format!(
            "    <item id=\"{}\" href=\"{}\" media-type=\"{}\"{}/>\n",
            id_of(p),
            href(p),
            media_type(p),
            if p.as_str() == "/nav.xhtml" {
                " properties=\"nav\""
            } else {
                ""
            }
        ));
    }
    let spine: String = pages
        .iter()
        .map(|(p, _)| format!("    <itemref idref=\"{}\"/>\n", id_of(p)))
        .collect();
    let opf = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<package xmlns=\"http://www.idpf.org/2007/opf\" version=\"3.0\" unique-identifier=\"uid\" xml:lang=\"{lang}\">\n  <metadata xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\n    <dc:identifier id=\"uid\">{}</dc:identifier>\n    <dc:title>{title}</dc:title>\n    <dc:language>{lang}</dc:language>\n    <meta property=\"dcterms:modified\">{}</meta>\n  </metadata>\n  <manifest>\n{items}  </manifest>\n  <spine>\n{spine}  </spine>\n</package>\n",
        uuid_of(&tree.digest()),
        iso_of(epoch)
    );
    let container = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<container version=\"1.0\" xmlns=\"urn:oasis:names:tc:opendocument:xmlns:container\">\n  <rootfiles>\n    <rootfile full-path=\"EPUB/package.opf\" media-type=\"application/oebps-package+xml\"/>\n  </rootfiles>\n</container>\n";
    let mut out: Vec<(String, Node)> = vec![
        (
            "/mimetype".into(),
            make_file(b"application/epub+zip".to_vec(), "epub", 0o644),
        ),
        (
            "/META-INF/container.xml".into(),
            make_file(container.as_bytes().to_vec(), "epub", 0o644),
        ),
        (
            "/EPUB/package.opf".into(),
            make_file(opf.into_bytes(), "epub", 0o644),
        ),
    ];
    if !site_nav {
        let list: String = pages
            .iter()
            .map(|(p, n)| {
                format!(
                    "<li><a href=\"{}\">{}</a></li>\n",
                    href(p),
                    escape_xml(&title_of(&n.bytes).unwrap_or_else(|| p[1..].to_string()))
                )
            })
            .collect();
        let nav = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE html>\n<html xmlns=\"http://www.w3.org/1999/xhtml\" xmlns:epub=\"http://www.idpf.org/2007/ops\" xml:lang=\"{lang}\" lang=\"{lang}\">\n<head><title>{title}</title></head>\n<body>\n<nav epub:type=\"toc\" id=\"toc\">\n<h1>{title}</h1>\n<ol>\n{list}</ol>\n</nav>\n</body>\n</html>\n"
        );
        out.push((
            "/EPUB/nav.xhtml".into(),
            make_file(nav.into_bytes(), "epub", 0o644),
        ));
    }
    for (p, n) in files {
        out.push((format!("/EPUB{p}"), n.clone()));
    }
    Tree::from_files(out)
}

/// `EXPORT … AS epub`: the EPUB file.
pub fn write_epub(tree: &Tree, epoch: Option<u64>) -> Result<Vec<u8>> {
    write_zip_first(
        &epub_tree(tree, epoch)?,
        &WriterOptions {
            epoch,
            dirs: Vec::new(),
        },
        &["/mimetype"],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(t: &str, b: &str) -> String {
        format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE html>\n<html xmlns=\"http://www.w3.org/1999/xhtml\" lang=\"fr\" xml:lang=\"fr\"><head><title>{t}</title></head><body><p>{b}</p></body></html>\n")
    }

    #[test]
    fn same_bytes_as_typescript() {
        let t = Tree::from_files(
            [
                ("/ch/1.xhtml", page("Un &amp; deux", "one")),
                ("/index.xhtml", page("Mon Livre", "début")),
                ("/style.css", "p{}".to_string()),
                ("/_headers", "/*\n  X: 1".to_string()),
            ]
            .into_iter()
            .map(|(p, s)| (p.to_string(), make_file(s.into_bytes(), "t", 0o644))),
        )
        .unwrap();
        assert_eq!(
            crate::digest::sha256(&write_epub(&t, None).unwrap()),
            "sha256:3036882396e82232d769aa6b154144e869f40a62c544386fbf3263a26ee3f7d4"
        );
        assert_eq!(
            crate::digest::sha256(&write_epub(&t, Some(1_700_000_000)).unwrap()),
            "sha256:9c5b0679da673591050204e35c34787d35d19fabfb4212f72fd0c449f69b1bda"
        );
        assert_eq!(
            escape_xml("a & b &amp; &#1; &#x1F; &#X1; <"),
            "a &amp; b &amp; &#1; &#x1F; &amp;#X1; &lt;"
        );
        assert_eq!(
            title_of(b"<TITLE lang=x>  A \n B </title>"),
            Some("A B".into())
        );
        assert_eq!(
            lang_of(b"<html xmlns=\"x\"\n  LANG = \"pt-BR\">"),
            Some("pt-BR".into())
        );
    }
}

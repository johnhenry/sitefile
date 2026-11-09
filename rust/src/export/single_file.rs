//! Single-file page exports, `EXPORT … AS html` and `AS mhtml` (port of `src/single-file.ts`,
//! issue #210; the rules are documented there and in docs/LANGUAGE.md).

use super::wbn::{mime_type_for, validate_base_url};
use crate::error::{ErrorCode::E_EXPORT, Result, SitefileError};
use crate::links::{encode_path, resolve_reference, scan_css, scan_html, Reference, Target};
use crate::tree::Tree;
use base64::Engine;
use std::collections::HashMap;

fn err(m: String) -> SitefileError {
    SitefileError::plain(E_EXPORT, m)
}

fn text<'a>(tree: &'a Tree, path: &str) -> Result<&'a str> {
    let n = tree
        .get(path)
        .ok_or_else(|| err(format!("single-file export: '{path}' does not exist")))?;
    std::str::from_utf8(&n.bytes)
        .map_err(|_| err(format!("single-file export: '{path}' is not valid UTF-8")))
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn media_type(path: &str) -> String {
    mime_type_for(path).replace(' ', "")
}

fn is_css(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(".css")
}

fn is_html(path: &str) -> bool {
    let l = path.to_ascii_lowercase();
    l.ends_with(".html") || l.ends_with(".htm")
}

/// The file a reference names when it is an asset to inline, with the URL's fragment.
fn asset_of(r: &Reference, from: &str) -> Result<Option<(String, String)>> {
    if matches!(r.element.as_deref(), Some("a href") | Some("area href")) {
        return Ok(None);
    }
    let Target::Internal { path, dir } = resolve_reference(&r.url, from, "/") else {
        return Ok(None);
    };
    if dir {
        return Err(err(format!(
            "single-file export: cannot inline '{}' in {from}:{}: it names a directory",
            r.url, r.line
        )));
    }
    if is_html(&path) {
        return Ok(None);
    }
    let fragment = r
        .url
        .find('#')
        .map(|i| r.url[i..].to_string())
        .unwrap_or_default();
    Ok(Some((path, fragment)))
}

fn missing(r: &Reference, from: &str, path: &str) -> SitefileError {
    err(format!(
        "single-file export: cannot inline '{}' in {from}:{}: no file {path} in the exported tree",
        r.url, r.line
    ))
}

fn page_of(tree: &Tree, page: Option<&str>) -> Result<String> {
    let p = page.unwrap_or("/index.html");
    if tree.get(p).is_none() {
        return Err(err(format!("single-file export: the exported tree has no {p} (export a directory with an index.html, or name the page: EXPORT /page.html AS html)")));
    }
    Ok(p.to_string())
}

struct Inliner<'a> {
    tree: &'a Tree,
    uris: HashMap<String, String>,
}

impl Inliner<'_> {
    fn data_uri(&mut self, path: &str, stack: &[String]) -> Result<String> {
        if let Some(u) = self.uris.get(path) {
            return Ok(u.clone());
        }
        let bytes = if is_css(path) {
            let mut s = stack.to_vec();
            s.push(path.to_string());
            self.inline_css(path, &s)?.into_bytes()
        } else {
            self.tree.get(path).expect("checked").bytes.to_vec()
        };
        let uri = format!("data:{};base64,{}", media_type(path), b64(&bytes));
        self.uris.insert(path.to_string(), uri.clone());
        Ok(uri)
    }

    fn rewrite(
        &mut self,
        src: &str,
        refs: &[Reference],
        from: &str,
        stack: &[String],
    ) -> Result<String> {
        let mut out = String::new();
        let mut at = 0;
        for r in refs {
            let Some((path, fragment)) = asset_of(r, from)? else {
                continue;
            };
            if self.tree.get(&path).is_none() {
                return Err(missing(r, from, &path));
            }
            if stack.contains(&path) {
                return Err(err(format!(
                    "single-file export: {from} imports {path}, which imports it back (a cycle)"
                )));
            }
            out.push_str(&src[at..r.start]);
            out.push_str(&self.data_uri(&path, stack)?);
            out.push_str(&fragment);
            at = r.end;
        }
        out.push_str(&src[at..]);
        Ok(out)
    }

    fn inline_css(&mut self, path: &str, stack: &[String]) -> Result<String> {
        let css = text(self.tree, path)?;
        self.rewrite(css, &scan_css(css), path, stack)
    }
}

/// `EXPORT … AS html`: the page with every asset inlined as a data: URI.
pub fn write_single_file(tree: &Tree, page: Option<&str>) -> Result<Vec<u8>> {
    let p = page_of(tree, page)?;
    let html = text(tree, &p)?;
    let mut inl = Inliner {
        tree,
        uris: HashMap::new(),
    };
    Ok(inl
        .rewrite(html, &scan_html(html), &p, std::slice::from_ref(&p))?
        .into_bytes())
}

pub const MHTML_DEFAULT_BASE: &str = "https://site.invalid/";
const BOUNDARY: &str = "----MultipartBoundary--sitefile----";

fn assets_of(tree: &Tree, page: &str) -> Result<Vec<String>> {
    fn visit(tree: &Tree, page: &str, from: &str, out: &mut Vec<String>) -> Result<()> {
        let src = text(tree, from)?;
        let refs = if from == page || !is_css(from) {
            scan_html(src)
        } else {
            scan_css(src)
        };
        for r in &refs {
            let Some((path, _)) = asset_of(r, from)? else {
                continue;
            };
            if tree.get(&path).is_none() {
                return Err(missing(r, from, &path));
            }
            if out.contains(&path) {
                continue;
            }
            out.push(path.clone());
            if is_css(&path) {
                visit(tree, page, &path, out)?;
            }
        }
        Ok(())
    }
    let mut out = vec![page.to_string()];
    visit(tree, page, page, &mut out)?;
    Ok(out)
}

/// `EXPORT … AS mhtml [BASE <url>]`: the page and its assets as MHTML.
pub fn write_mhtml(tree: &Tree, page: Option<&str>, base_url: Option<&str>) -> Result<Vec<u8>> {
    let p = page_of(tree, page)?;
    let base = validate_base_url(Some(base_url.unwrap_or(MHTML_DEFAULT_BASE)))?;
    let url = |path: &str| format!("{base}{}", encode_path(&path[1..]));
    let mut out = format!(
        "From: <Saved by sitefile>\r\nSnapshot-Content-Location: {}\r\nMIME-Version: 1.0\r\nContent-Type: multipart/related;\r\n\ttype=\"text/html\";\r\n\tboundary=\"{BOUNDARY}\"\r\n\r\n",
        url(&p)
    );
    for path in assets_of(tree, &p)? {
        out.push_str(&format!(
            "--{BOUNDARY}\r\nContent-Type: {}\r\nContent-Transfer-Encoding: base64\r\nContent-Location: {}\r\n\r\n",
            mime_type_for(&path),
            url(&path)
        ));
        let b = b64(&tree.get(&path).expect("checked").bytes);
        for chunk in b.as_bytes().chunks(76) {
            out.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
            out.push_str("\r\n");
        }
        out.push_str("\r\n");
    }
    out.push_str(&format!("--{BOUNDARY}--\r\n"));
    Ok(out.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::make_file;

    fn tree(files: &[(&str, &str)]) -> Tree {
        Tree::from_files(
            files
                .iter()
                .map(|(p, s)| (p.to_string(), make_file(s.as_bytes().to_vec(), "t", 0o644))),
        )
        .unwrap()
    }

    #[test]
    fn inlines_and_keeps() {
        let t = tree(&[
            (
                "/index.html",
                "<link rel=stylesheet href=a.css><a href=b.png>x</a><img src=b.png>",
            ),
            ("/a.css", "p{background:url(b.png)}"),
            ("/b.png", "B"),
        ]);
        let out = String::from_utf8(write_single_file(&t, None).unwrap()).unwrap();
        let png = format!("data:image/png;base64,{}", b64(b"B"));
        let css = format!(
            "data:text/css;charset=utf-8;base64,{}",
            b64(format!("p{{background:url({png})}}").as_bytes())
        );
        assert_eq!(
            out,
            format!("<link rel=stylesheet href={css}><a href=b.png>x</a><img src={png}>")
        );
        let e = write_single_file(&tree(&[("/index.html", "<img src=x.png>")]), None).unwrap_err();
        assert_eq!(
            e.message(),
            "single-file export: cannot inline 'x.png' in /index.html:1: no file /x.png in the exported tree"
        );
        let m = String::from_utf8(write_mhtml(&t, None, None).unwrap()).unwrap();
        assert!(m.contains("Content-Location: https://site.invalid/a.css\r\n"));
        assert!(m.ends_with("------MultipartBoundary--sitefile------\r\n"));
    }
}

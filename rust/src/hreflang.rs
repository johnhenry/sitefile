//! LANGUAGES (port of `src/hreflang.ts`; docs/LANGUAGE.md, "Site files"): which pages translate each
//! other, for the hreflang alternates CANONICAL and SITEMAP write. Groups and messages match the TypeScript.

use crate::error::{ErrorCode::E_VALIDATION, Result, SitefileError, SourceSpan};
use std::collections::HashMap;

/// A basic BCP 47 check: a 2-3 letter language, then `-` subtags of 1-8 letters or digits.
pub fn is_language_tag(s: &str) -> bool {
    let mut parts = s.split('-');
    let first = parts.next().unwrap_or("");
    if !(2..=3).contains(&first.len()) || !first.bytes().all(|b| b.is_ascii_alphabetic()) {
        return false;
    }
    parts.all(|p| (1..=8).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// What a document's frontmatter says about the page it renders: `lang` and `translationKey`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Translation {
    pub lang: String,
    pub key: String,
}

/// LANGUAGES, evaluated, with the translations documents give (tree path -> translation).
#[derive(Debug, Clone, Default)]
pub struct Languages {
    pub codes: Vec<String>,
    pub default: Option<String>,
    pub docs: HashMap<String, Translation>,
}

/// One alternate of a page: its `hreflang` value and the URL path of the translation.
pub type Alternate = (String, String);

struct Member {
    path: String,
    url: String,
    lang: String,
}

/// The alternates of every page that has a translation (`alternates` in src/hreflang.ts). `pages` are
/// (tree path, URL path) pairs.
pub fn alternates(
    pages: &[(String, String)],
    langs: &Languages,
    what: &str,
    span: &SourceSpan,
) -> Result<HashMap<String, Vec<Alternate>>> {
    let mut sorted: Vec<&(String, String)> = pages.iter().collect();
    sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<Member>> = HashMap::new();
    for (path, url) in sorted {
        let (group, lang, label) = if let Some(doc) = langs.docs.get(path) {
            (
                format!("k\0{}", doc.key),
                doc.lang.clone(),
                format!("translationKey '{}'", doc.key),
            )
        } else {
            let folder = url[1.min(url.len())..]
                .find('/')
                .map(|i| (&url[1..i + 1], i + 1));
            match folder {
                Some((f, slash)) if langs.codes.iter().any(|c| c == f) => (
                    format!("f\0{}", &url[slash..]),
                    f.to_string(),
                    format!("'{}'", &url[slash..]),
                ),
                _ => match &langs.default {
                    Some(d) => (format!("f\0{url}"), d.clone(), format!("'{url}'")),
                    None => continue,
                },
            }
        };
        let members = groups.entry(group.clone()).or_insert_with(|| {
            order.push(group);
            Vec::new()
        });
        if let Some(same) = members.iter().find(|m| m.lang.eq_ignore_ascii_case(&lang)) {
            return Err(SitefileError::at(
                E_VALIDATION,
                format!(
                    "{what}: {} and {path} are both the '{lang}' page of {label}; a translation group has one page per language",
                    same.path
                ),
                span,
            ));
        }
        members.push(Member {
            path: path.clone(),
            url: url.clone(),
            lang,
        });
    }
    let mut out = HashMap::new();
    let dflt = langs.default.as_ref().map(|d| d.to_ascii_lowercase());
    for g in &order {
        let members = &groups[g];
        if members.len() < 2 {
            continue;
        }
        let mut by_lang: Vec<&Member> = members.iter().collect();
        by_lang.sort_by_key(|m| m.lang.to_ascii_lowercase());
        let mut alts: Vec<Alternate> = by_lang
            .iter()
            .map(|m| (m.lang.clone(), m.url.clone()))
            .collect();
        if let Some(d) = by_lang
            .iter()
            .find(|m| Some(m.lang.to_ascii_lowercase()) == dflt)
        {
            alts.push(("x-default".into(), d.url.clone()));
        }
        for m in members {
            out.insert(m.path.clone(), alts.clone());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pages(ps: &[(&str, &str)]) -> Vec<(String, String)> {
        ps.iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn language_tags() {
        for ok in ["en", "fr", "pt-BR", "zh-Hant-TW", "es-419", "EN"] {
            assert!(is_language_tag(ok), "{ok}");
        }
        for bad in [
            "",
            "e",
            "english",
            "en_US",
            "en-",
            "x-default",
            "en-toolongtag",
            "1a",
        ] {
            assert!(!is_language_tag(bad), "{bad}");
        }
    }

    #[test]
    fn folders_documents_and_duplicates() {
        let span = SourceSpan::new("Sitefile", 3, 1);
        let mut langs = Languages {
            codes: vec!["en".into(), "fr".into()],
            default: Some("en".into()),
            docs: HashMap::new(),
        };
        langs.docs.insert(
            "/blog/b.html".into(),
            Translation {
                lang: "fr".into(),
                key: "k".into(),
            },
        );
        langs.docs.insert(
            "/blog/a.html".into(),
            Translation {
                lang: "en".into(),
                key: "k".into(),
            },
        );
        let out = alternates(
            &pages(&[
                ("/fr/x.html", "/fr/x.html"),
                ("/x.html", "/x.html"),
                ("/blog/a.html", "/blog/a.html"),
                ("/blog/b.html", "/blog/b.html"),
                ("/only.html", "/only.html"),
            ]),
            &langs,
            "SITEMAP",
            &span,
        )
        .unwrap();
        let alt = |l: &str, u: &str| (l.to_string(), u.to_string());
        assert_eq!(
            out["/x.html"],
            [
                alt("en", "/x.html"),
                alt("fr", "/fr/x.html"),
                alt("x-default", "/x.html")
            ]
        );
        assert_eq!(
            out["/blog/b.html"],
            [
                alt("en", "/blog/a.html"),
                alt("fr", "/blog/b.html"),
                alt("x-default", "/blog/a.html")
            ]
        );
        assert!(!out.contains_key("/only.html"));
        let e = alternates(
            &pages(&[("/en/x.html", "/en/x.html"), ("/x.html", "/x.html")]),
            &langs,
            "CANONICAL",
            &span,
        )
        .unwrap_err();
        assert_eq!(e.detail, "CANONICAL: /en/x.html and /x.html are both the 'en' page of '/x.html'; a translation group has one page per language");
    }
}

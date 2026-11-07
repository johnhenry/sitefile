//! Bytes to resources, and integrity checks (port of `src/resources/convert.ts`).

use crate::digest::sha256;
use crate::error::{ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::language::ast::ImportKind;
use crate::resources::archive::extract_archive;
use crate::resources::bundle_data::bundle_value;
use crate::resources::types::{ArchiveLimits, Provenance, Resource, ValueFormat};
use crate::resources::wbn_reader::read_web_bundle;

use crate::csv::parse_csv_records;
use crate::documents::markdown_document;
use crate::yaml::parse_yaml;

#[derive(Debug, Clone, Default)]
pub struct BytesInfo {
    pub uri: String,
    pub resolved_url: Option<String>,
    pub basename: String,
    pub mode: Option<u32>,
}

fn decode_utf8<'a>(bytes: &'a [u8], uri: &str, span: Option<&SourceSpan>) -> Result<&'a str> {
    match std::str::from_utf8(bytes) {
        // TextDecoder strips a leading BOM.
        Ok(s) => Ok(s.strip_prefix('\u{feff}').unwrap_or(s)),
        Err(_) => Err(SitefileError::new(E_ENCODING, format!("{uri} is not valid UTF-8"), span.cloned())),
    }
}

/// Turn fetched or read bytes into a resource of the requested kind.
pub fn bytes_to_resource(
    bytes: Vec<u8>,
    kind: ImportKind,
    info: &BytesInfo,
    span: Option<&SourceSpan>,
    limits: &ArchiveLimits,
) -> Result<Resource> {
    let digest = sha256(&bytes);
    let provenance = Provenance { uri: info.uri.clone(), resolved_url: info.resolved_url.clone(), digest, git_commit: None };
    let value = |format: ValueFormat, value: serde_json::Value, provenance: Provenance| Resource::Value { format, value, provenance };
    match kind {
        // The provenance digest is that of the archive bytes, which is what pins and the lockfile cover.
        ImportKind::Tree => return Ok(Resource::Tree { tree: extract_archive(&bytes, &info.uri, limits, span)?, provenance }),
        ImportKind::Markdown => {
            let doc = markdown_document(&bytes, &format!("/{}", info.basename), &info.uri, span)?;
            return Ok(value(ValueFormat::Markdown, doc, provenance));
        }
        ImportKind::Wbn => {
            let v = bundle_value(&read_web_bundle(&bytes, &info.uri, limits, span)?);
            return Ok(value(ValueFormat::Wbn, v, provenance));
        }
        ImportKind::File => {
            let mode = if info.mode.unwrap_or(0o644) & 0o111 != 0 { 0o755 } else { 0o644 };
            return Ok(Resource::File { bytes, mode, basename: info.basename.clone(), provenance });
        }
        _ => {}
    }
    let text = decode_utf8(&bytes, &info.uri, span)?;
    Ok(match kind {
        ImportKind::Text => value(ValueFormat::Text, serde_json::Value::String(text.to_string()), provenance),
        // A file-based html fragment: its template is parsed where a RENDER uses it.
        ImportKind::Html => value(ValueFormat::Html, serde_json::Value::String(text.to_string()), provenance),
        ImportKind::Yaml => value(ValueFormat::Yaml, parse_yaml(text, &info.uri)?, provenance),
        ImportKind::Csv => value(ValueFormat::Csv, parse_csv_records(text, &info.uri)?, provenance),
        _ => {
            let v: serde_json::Value = serde_json::from_str(text).map_err(|e| {
                SitefileError::new(E_TYPE, format!("{} is not valid JSON: {e}", info.uri), span.cloned())
            })?;
            value(ValueFormat::Json, v, provenance)
        }
    })
}

/// E_INTEGRITY unless sha256(bytes) equals `expected` (`sha256:<hex>`).
pub fn verify_integrity(bytes: &[u8], expected: &str, uri: &str, span: Option<&SourceSpan>) -> Result<()> {
    let actual = sha256(bytes);
    if actual != expected.to_lowercase() {
        return Err(SitefileError::new(
            E_INTEGRITY,
            format!("integrity mismatch for {uri}: expected {expected}, got {actual}"),
            span.cloned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::types::DEFAULT_ARCHIVE_LIMITS as L;

    #[test]
    fn bytes_to_resource_and_verify_integrity() {
        let info = BytesInfo { uri: "https://x/a.json".into(), resolved_url: Some("https://cdn/a.json".into()), basename: "a.json".into(), mode: None };
        let bytes = br#"{"a":[1,2]}"#.to_vec();
        let f = bytes_to_resource(bytes.clone(), ImportKind::File, &BytesInfo { mode: Some(0o700), ..info.clone() }, None, &L).unwrap();
        match &f {
            Resource::File { mode, basename, provenance, .. } => {
                assert_eq!(*mode, 0o755);
                assert_eq!(basename, "a.json");
                assert_eq!(provenance.resolved_url.as_deref(), Some("https://cdn/a.json"));
                assert_eq!(provenance.digest, sha256(&bytes));
            }
            _ => panic!("expected file"),
        }
        match bytes_to_resource(bytes.clone(), ImportKind::Json, &info, None, &L).unwrap() {
            Resource::Value { format, value, .. } => {
                assert_eq!(format, ValueFormat::Json);
                assert_eq!(value, serde_json::json!({ "a": [1, 2] }));
            }
            _ => panic!("expected value"),
        }
        let plain = BytesInfo { uri: "./a".into(), basename: "a".into(), ..Default::default() };
        match bytes_to_resource(bytes.clone(), ImportKind::Text, &plain, None, &L).unwrap() {
            Resource::Value { format, value, provenance } => {
                assert_eq!(format, ValueFormat::Text);
                assert_eq!(value, serde_json::Value::String(r#"{"a":[1,2]}"#.into()));
                assert!(provenance.resolved_url.is_none());
            }
            _ => panic!("expected value"),
        }
        let code = |b: &[u8], k| bytes_to_resource(b.to_vec(), k, &info, None, &L).unwrap_err().code;
        assert_eq!(code(&[0xff, 0xfe], ImportKind::Text), E_ENCODING);
        assert_eq!(code(&[0xc3], ImportKind::Json), E_ENCODING);
        assert_eq!(code(b"{nope", ImportKind::Json), E_TYPE);

        verify_integrity(&bytes, &sha256(&bytes), &info.uri, None).unwrap();
        verify_integrity(&bytes, &sha256(&bytes).to_uppercase().replace("SHA256", "sha256"), &info.uri, None).unwrap();
        let wrong = format!("sha256:{}", "0".repeat(64));
        let e = verify_integrity(&bytes, &wrong, &info.uri, None).unwrap_err();
        assert_eq!(e.code, E_INTEGRITY);
        assert!(e.message().contains(&wrong) && e.message().contains(&sha256(&bytes)));
    }

    #[test]
    fn text_strips_a_bom_and_tree_extracts_archives() {
        let plain = BytesInfo { uri: "./a".into(), basename: "a".into(), ..Default::default() };
        match bytes_to_resource(b"\xef\xbb\xbfhi".to_vec(), ImportKind::Text, &plain, None, &L).unwrap() {
            Resource::Value { value, .. } => assert_eq!(value, "hi"),
            _ => panic!(),
        }
        let tar = crate::resources::archive::tests::tar(&[crate::resources::archive::tests::te("x.txt", "x")]);
        match bytes_to_resource(tar.clone(), ImportKind::Tree, &plain, None, &L).unwrap() {
            Resource::Tree { tree, provenance } => {
                assert!(tree.is_file("/x.txt"));
                assert_eq!(provenance.digest, sha256(&tar));
            }
            _ => panic!(),
        }
    }
}

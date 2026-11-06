//! Build attestations for OCI site images (`build --provenance` / `--sbom`);
//! port of `src/attest.ts`, byte for byte.
//!
//! Each attestation is an in-toto Statement v1 (one JSON blob) wrapped in an
//! OCI 1.1 artifact manifest whose `subject` is the image manifest
//! (artifactType `application/vnd.in-toto+json`, the empty config, one layer
//! annotated `in-toto.io/predicate-type`):
//!
//! - provenance: SLSA provenance v1 (Sitefile digest, build args except
//!   secret-looking names, imports with digests, BASE digest, builder version)
//! - sbom: a CycloneDX 1.6 document listing every file of the site layer
//!   (path in the image, SHA-256, mode, origin) and the BASE image
//!
//! The image manifest is never touched, so the image ID is the same with or
//! without attestations. No timestamps unless SOURCE_DATE_EPOCH is set.

use crate::digest::sha256;
use crate::error::{ErrorCode::*, Result, SitefileError};
use crate::executor::{relative_path, BuildResult};
use crate::export::exporter::resolve_path;
use crate::export::oci::{canonical_json, iso_of, media};
use crate::tree::Tree;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::Path;

/// Sitefile version recorded as the builder version (also `sitefile --version`).
pub const SITEFILE_VERSION: &str = "0.1.0";

pub const IN_TOTO: &str = "application/vnd.in-toto+json";
pub const EMPTY_CONFIG: &str = "application/vnd.oci.empty.v1+json";
pub const PREDICATE_ANNOTATION: &str = "in-toto.io/predicate-type";
pub const STATEMENT: &str = "https://in-toto.io/Statement/v1";
pub const SLSA: &str = "https://slsa.dev/provenance/v1";
pub const CYCLONEDX: &str = "https://cyclonedx.org/bom";
pub const BUILD_TYPE: &str = "https://github.com/johnhenry/sitefile/blob/main/docs/CLI.md#attestations";
pub const BUILDER_ID: &str = "https://github.com/johnhenry/sitefile";

/// The OCI empty descriptor's blob (`{}`).
pub const EMPTY_JSON: &[u8] = b"{}";

pub fn empty_digest() -> String {
    sha256(EMPTY_JSON)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AttestRequest {
    pub provenance: bool,
    pub sbom: bool,
}

impl AttestRequest {
    pub fn any(&self) -> bool {
        self.provenance || self.sbom
    }
}

/// An image-index descriptor of a referrer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferrerDescriptor {
    pub media_type: String,
    pub digest: String,
    pub size: u64,
    pub artifact_type: Option<String>,
    pub annotations: Option<BTreeMap<String, String>>,
}

impl ReferrerDescriptor {
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("mediaType".into(), Value::String(self.media_type.clone()));
        m.insert("digest".into(), Value::String(self.digest.clone()));
        m.insert("size".into(), Value::from(self.size));
        if let Some(t) = &self.artifact_type {
            m.insert("artifactType".into(), Value::String(t.clone()));
        }
        if let Some(a) = &self.annotations {
            m.insert("annotations".into(), Value::Object(a.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect()));
        }
        Value::Object(m)
    }

    pub fn predicate_type(&self) -> Option<&str> {
        self.annotations.as_ref().and_then(|a| a.get(PREDICATE_ANNOTATION)).map(String::as_str)
    }
}

/// One attestation (or any referrer): its descriptor and the blobs it needs
/// (the manifest, its config and its layers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Referrer {
    pub descriptor: ReferrerDescriptor,
    pub blobs: Vec<(String, Vec<u8>)>,
}

/// What the provenance and SBOM say about a build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildFacts {
    /// The Sitefile's path relative to the context (its basename when elsewhere).
    pub sitefile: String,
    pub sitefile_digest: String,
    /// Build args (JavaScript key order), except secret-looking names.
    pub args: Vec<(String, String)>,
    pub redacted_args: Vec<String>,
    pub platform: String,
    pub imports: Vec<ImportFact>,
    pub base: Option<(String, String)>,
    /// `--strict-pins`: how each remote input was pinned (shown in the provenance).
    pub pins: Option<crate::executor::Pins>,
    /// File origins start with the Sitefile label (relative to the cwd).
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportFact {
    pub name: String,
    pub uri: String,
    pub resolved: Option<String>,
    pub digest: String,
}

/// The image an attestation is about.
pub struct AttestSubject<'a> {
    pub digest: &'a str,
    pub size: u64,
    pub files: &'a Tree,
    pub root: &'a str,
    pub title: Option<&'a str>,
}

const SECRET_WORDS: &[&str] = &["apikey", "auth", "credential", "credentials", "key", "passwd", "password", "pword", "secret", "token"];

/// Whether a build-arg name looks like it holds a secret (any `_`-separated word is key, token, password, ...).
pub fn is_secret_arg_name(name: &str) -> bool {
    name.to_lowercase().split('_').any(|w| SECRET_WORDS.contains(&w))
}

/// The Sitefile path the build used, as `build` resolves it.
pub fn sitefile_path_of(context_dir: &str, sitefile: Option<&str>) -> std::path::PathBuf {
    let given = match sitefile {
        Some(s) => s.to_string(),
        None => crate::executor::join_path(context_dir, "Sitefile"),
    };
    resolve_path(Path::new(&given))
}

/// Collect the facts for a finished build. Reads the Sitefile again to digest it.
pub fn build_facts(result: &BuildResult, context_dir: &str, sitefile: Option<&str>, build_args: &crate::build_args::BuildArgs, platform: Option<&str>) -> Result<BuildFacts> {
    let file = sitefile_path_of(context_dir, sitefile);
    let bytes = std::fs::read(&file).map_err(|_| SitefileError::plain(E_NOT_FOUND, format!("cannot read Sitefile '{}'", file.display())))?;
    let base_name = file.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let rel = relative_path(&resolve_path(Path::new(context_dir)), &file);
    let inside = !rel.is_empty() && !rel.starts_with("..") && !rel.starts_with('/');
    let sitefile = if inside { rel } else { base_name.clone() };
    let mut args = Vec::new();
    let mut redacted_args = Vec::new();
    for (k, v) in build_args.iter() {
        if is_secret_arg_name(k) {
            redacted_args.push(k.to_string());
        } else {
            args.push((k.to_string(), v.to_string()));
        }
    }
    redacted_args.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
    let imports = result
        .resources
        .iter()
        .map(|(name, r)| {
            let p = r.provenance();
            ImportFact { name: name.clone(), uri: p.uri.clone(), resolved: p.resolved_url.clone(), digest: p.digest.clone() }
        })
        .collect();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
    let label_rel = relative_path(&cwd, &file);
    Ok(BuildFacts {
        sitefile,
        sitefile_digest: sha256(&bytes),
        args,
        redacted_args,
        platform: platform.unwrap_or("linux/amd64").to_string(),
        imports,
        base: result.base.as_ref().map(|b| (b.name.clone(), b.manifest_digest.clone())),
        pins: result.pins.clone(),
        label: if label_rel.is_empty() { base_name } else { label_rel },
    })
}

fn hex(digest: &str) -> &str {
    digest.strip_prefix("sha256:").unwrap_or(digest)
}

/// SLSA provenance v1 predicate.
pub fn provenance_predicate(facts: &BuildFacts, epoch: Option<u64>) -> Value {
    let args: Map<String, Value> = facts.args.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect();
    let mut external = Map::new();
    external.insert("sitefile".into(), Value::String(facts.sitefile.clone()));
    external.insert("args".into(), Value::Object(args));
    external.insert("platform".into(), Value::String(facts.platform.clone()));
    if !facts.redacted_args.is_empty() {
        external.insert("redactedArgs".into(), json!(facts.redacted_args));
    }
    // --strict-pins: the build refused unpinned inputs; each remote dependency says what pinned it.
    if facts.pins.is_some() {
        external.insert("strictPins".into(), Value::Bool(true));
    }
    let mut deps = vec![json!({ "name": "sitefile", "uri": facts.sitefile, "digest": { "sha256": hex(&facts.sitefile_digest) } })];
    for i in &facts.imports {
        let mut d = json!({ "name": format!("import:{}", i.name), "uri": i.uri, "digest": { "sha256": hex(&i.digest) } });
        if let Some(r) = i.resolved.as_ref().filter(|r| **r != i.uri) {
            d["downloadLocation"] = Value::String(r.clone());
        }
        if let Some(pin) = facts.pins.as_ref().and_then(|p| p.import(&i.name)) {
            d["annotations"] = json!({ "dev.sitefile.pin": pin });
        }
        deps.push(d);
    }
    if let Some((name, digest)) = &facts.base {
        let mut d = json!({ "name": "base", "uri": name, "digest": { "sha256": hex(digest) } });
        if let Some(pin) = facts.pins.as_ref().and_then(|p| p.base) {
            d["annotations"] = json!({ "dev.sitefile.pin": pin });
        }
        deps.push(d);
    }
    let mut run = json!({ "builder": { "id": BUILDER_ID, "version": { "sitefile": SITEFILE_VERSION } } });
    if let Some(e) = epoch {
        run["metadata"] = json!({ "startedOn": iso_of(Some(e)), "finishedOn": iso_of(Some(e)) });
    }
    json!({ "buildDefinition": { "buildType": BUILD_TYPE, "externalParameters": external, "resolvedDependencies": deps }, "runDetails": run })
}

/// CycloneDX 1.6 SBOM: the image, its BASE and every file of the site layer.
pub fn sbom_predicate(subject: &AttestSubject<'_>, facts: &BuildFacts, epoch: Option<u64>) -> Value {
    let mut metadata = json!({
        "tools": { "components": [{ "type": "application", "name": "sitefile", "version": SITEFILE_VERSION }] },
        "component": { "type": "container", "bom-ref": "image", "name": subject.title.unwrap_or("site"), "version": subject.digest },
    });
    if let Some(e) = epoch {
        metadata["timestamp"] = Value::String(iso_of(Some(e)));
    }
    let mut components = Vec::new();
    if let Some((name, digest)) = &facts.base {
        components.push(json!({ "type": "container", "bom-ref": "base", "name": name, "version": digest }));
    }
    let prefix = format!("{}:", facts.label);
    for (p, n) in subject.files.entries() {
        let name = if subject.root == "/" { p.clone() } else { format!("{}{p}", subject.root) };
        let origin = match n.origin.strip_prefix(&prefix) {
            Some(rest) => format!("{}:{rest}", facts.sitefile),
            None => n.origin.clone(),
        };
        components.push(json!({
            "type": "file",
            "bom-ref": format!("file:{name}"),
            "name": name,
            "hashes": [{ "alg": "SHA-256", "content": hex(&n.digest) }],
            "properties": [
                { "name": "sitefile:mode", "value": format!("{:o}", n.mode) },
                { "name": "sitefile:origin", "value": origin },
            ],
        }));
    }
    json!({ "bomFormat": "CycloneDX", "specVersion": "1.6", "version": 1, "metadata": metadata, "components": components })
}

/// Wrap a predicate in an in-toto statement about `subject` and an OCI artifact manifest.
pub fn attestation_referrer(subject_digest: &str, subject_size: u64, predicate_type: &str, predicate: Value) -> Referrer {
    let statement = canonical_json(&json!({
        "_type": STATEMENT,
        "subject": [{ "name": "_", "digest": { "sha256": hex(subject_digest) } }],
        "predicateType": predicate_type,
        "predicate": predicate,
    }))
    .into_bytes();
    let layer_digest = sha256(&statement);
    let empty = empty_digest();
    let mut annotations = Map::new();
    annotations.insert(PREDICATE_ANNOTATION.into(), Value::String(predicate_type.into()));
    let annotations = Value::Object(annotations);
    let manifest = canonical_json(&json!({
        "schemaVersion": 2,
        "mediaType": media::MANIFEST,
        "artifactType": IN_TOTO,
        "config": { "mediaType": EMPTY_CONFIG, "digest": empty, "size": EMPTY_JSON.len() },
        "layers": [{ "mediaType": IN_TOTO, "digest": layer_digest, "size": statement.len(), "annotations": annotations }],
        "subject": { "mediaType": media::MANIFEST, "digest": subject_digest, "size": subject_size },
        "annotations": annotations,
    }))
    .into_bytes();
    let digest = sha256(&manifest);
    let mut ann = BTreeMap::new();
    ann.insert(PREDICATE_ANNOTATION.to_string(), predicate_type.to_string());
    Referrer {
        descriptor: ReferrerDescriptor { media_type: media::MANIFEST.into(), digest: digest.clone(), size: manifest.len() as u64, artifact_type: Some(IN_TOTO.into()), annotations: Some(ann) },
        blobs: vec![(digest, manifest), (empty, EMPTY_JSON.to_vec()), (layer_digest, statement)],
    }
}

/// The requested attestations for an image, provenance first.
pub fn make_attestations(subject: &AttestSubject<'_>, facts: &BuildFacts, request: AttestRequest, epoch: Option<u64>) -> Vec<Referrer> {
    let mut out = Vec::new();
    if request.provenance {
        out.push(attestation_referrer(subject.digest, subject.size, SLSA, provenance_predicate(facts, epoch)));
    }
    if request.sbom {
        out.push(attestation_referrer(subject.digest, subject.size, CYCLONEDX, sbom_predicate(subject, facts, epoch)));
    }
    out
}

/// An OCI image index listing referrers (the store's referrers file and the registry fallback tag).
pub fn encode_referrers_index(descriptors: &[ReferrerDescriptor]) -> Vec<u8> {
    let manifests: Vec<Value> = descriptors.iter().map(ReferrerDescriptor::to_json).collect();
    canonical_json(&json!({ "schemaVersion": 2, "mediaType": media::INDEX, "manifests": manifests })).into_bytes()
}

/// Parse a referrers index; entries that are not well-formed descriptors are dropped.
pub fn decode_referrers_index(bytes: &[u8]) -> std::result::Result<Vec<ReferrerDescriptor>, String> {
    let j: Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let list = match (j.get("schemaVersion").and_then(Value::as_u64), j.get("manifests").and_then(Value::as_array)) {
        (Some(2), Some(m)) => m,
        _ => return Err("not an OCI image index".into()),
    };
    let mut out = Vec::new();
    for m in list {
        let Some(digest) = m.get("digest").and_then(Value::as_str).filter(|d| crate::digest::is_integrity(d)) else { continue };
        let Some(size) = m.get("size").and_then(Value::as_u64).filter(|s| *s <= 9_007_199_254_740_991) else { continue };
        let annotations = m.get("annotations").and_then(Value::as_object).map(|a| a.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect());
        out.push(ReferrerDescriptor {
            media_type: m.get("mediaType").and_then(Value::as_str).unwrap_or(media::MANIFEST).to_string(),
            digest: digest.to_string(),
            size,
            artifact_type: m.get("artifactType").and_then(Value::as_str).map(str::to_string),
            annotations,
        });
    }
    Ok(out)
}

/// The referrers fallback tag for a subject digest (`sha256-<hex>`, distribution spec v1.1).
pub fn referrers_tag(subject: &str) -> String {
    subject.replacen(':', "-", 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{make_file, Tree};

    fn facts() -> BuildFacts {
        BuildFacts {
            sitefile: "Sitefile".into(),
            sitefile_digest: sha256(b"FROM scratch\n"),
            args: vec![("TITLE".into(), "Docs".into())],
            redacted_args: vec!["API_TOKEN".into()],
            platform: "linux/amd64".into(),
            imports: vec![ImportFact { name: "data".into(), uri: "https://x.example/d.json".into(), resolved: Some("https://cdn.example/d.json".into()), digest: sha256(b"{}") }],
            base: None,
            pins: None,
            label: "ctx/Sitefile".into(),
        }
    }

    #[test]
    fn strict_pins_are_recorded_in_the_provenance() {
        let plain = provenance_predicate(&facts(), None);
        assert!(plain["buildDefinition"]["externalParameters"].get("strictPins").is_none());
        assert!(plain["buildDefinition"]["resolvedDependencies"][1].get("annotations").is_none());
        let pins = crate::executor::Pins { imports: vec![("data".into(), "integrity")], base: Some("digest") };
        let f = BuildFacts { pins: Some(pins), base: Some(("r.example/base:1".into(), sha256(b"m"))), ..facts() };
        let p = provenance_predicate(&f, None);
        assert_eq!(p["buildDefinition"]["externalParameters"]["strictPins"], json!(true));
        let deps = &p["buildDefinition"]["resolvedDependencies"];
        assert_eq!(deps[1]["annotations"], json!({ "dev.sitefile.pin": "integrity" }));
        assert_eq!(deps[2]["annotations"], json!({ "dev.sitefile.pin": "digest" }));
    }

    #[test]
    fn secret_names() {
        for n in ["API_TOKEN", "token", "DB_PASSWORD", "AWS_SECRET_ACCESS_KEY", "NPM_AUTH", "GITHUB_APIKEY", "my_credentials"] {
            assert!(is_secret_arg_name(n), "{n}");
        }
        for n in ["TITLE", "TOKENIZER", "KEYBOARD", "MONKEY", "AUTHOR", "SITE_URL"] {
            assert!(!is_secret_arg_name(n), "{n}");
        }
        assert_eq!(referrers_tag(&format!("sha256:{}", "a".repeat(64))), format!("sha256-{}", "a".repeat(64)));
    }

    #[test]
    fn provenance_shape_and_epoch() {
        let p = provenance_predicate(&facts(), None);
        let s = canonical_json(&p);
        assert!(s.contains(r#""externalParameters":{"args":{"TITLE":"Docs"},"platform":"linux/amd64","redactedArgs":["API_TOKEN"],"sitefile":"Sitefile"}"#), "{s}");
        assert!(s.contains(r#""downloadLocation":"https://cdn.example/d.json""#));
        assert!(!s.contains("startedOn"));
        let p = provenance_predicate(&facts(), Some(1_700_000_000));
        assert_eq!(p["runDetails"]["metadata"]["startedOn"], "2023-11-14T22:13:20Z");
    }

    #[test]
    fn sbom_rewrites_origins_and_roots_paths() {
        let tree = Tree::from_files(vec![("/a.html".to_string(), make_file(b"a".to_vec(), "ctx/Sitefile:3 COPY", 0o644))]).unwrap();
        let subject = AttestSubject { digest: "sha256:00", size: 1, files: &tree, root: "/srv/www", title: Some("ctx") };
        let b = sbom_predicate(&subject, &facts(), None);
        assert_eq!(b["components"][0]["name"], "/srv/www/a.html");
        assert_eq!(b["components"][0]["properties"][1]["value"], "Sitefile:3 COPY");
        assert!(b["metadata"].get("timestamp").is_none());
    }

    #[test]
    fn referrer_manifest_points_at_the_subject_and_round_trips() {
        let r = attestation_referrer(&format!("sha256:{}", "c".repeat(64)), 509, SLSA, json!({}));
        let m: Value = serde_json::from_slice(&r.blobs[0].1).unwrap();
        assert_eq!(m["subject"]["size"], 509);
        assert_eq!(m["artifactType"], IN_TOTO);
        assert_eq!(m["config"]["digest"], empty_digest());
        assert_eq!(empty_digest(), "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a");
        let idx = encode_referrers_index(&[r.descriptor.clone()]);
        assert_eq!(decode_referrers_index(&idx).unwrap(), vec![r.descriptor]);
    }
}

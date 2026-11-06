//! `Sitefile.lock`: pinned digests of remote resources and BASE images
//! (port of `src/resources/lockfile.ts`).

use crate::digest::is_integrity;
use crate::error::{ErrorCode::*, Result, SitefileError};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockEntry {
    /// `sha256:<hex>` of the response body.
    pub integrity: String,
    /// Body size in bytes.
    pub size: u64,
    /// Final URL after redirects.
    pub resolved: String,
}

/// A container image reference (`BASE`) pinned to the digest its tag resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageLockEntry {
    /// `sha256:<hex>` of what the tag pointed at: an image index or a manifest.
    pub digest: String,
}

/// A Git IMPORT (keyed by the URI as written) pinned to the commit its ref resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitLockEntry {
    /// 40 lowercase hex digits.
    pub commit: String,
}

#[derive(Debug, Clone)]
pub struct Lockfile {
    pub path: PathBuf,
    entries: BTreeMap<String, LockEntry>,
    used: BTreeSet<String>,
    images: BTreeMap<String, ImageLockEntry>,
    used_images: BTreeSet<String>,
    gits: BTreeMap<String, GitLockEntry>,
    used_gits: BTreeSet<String>,
    existed: bool,
    original_text: Option<String>,
}

/// `Number.isSafeInteger(v) && v >= 0`
fn safe_size(v: &Value) -> Option<u64> {
    const MAX_SAFE: u64 = (1 << 53) - 1;
    if let Some(n) = v.as_u64() {
        return (n <= MAX_SAFE).then_some(n);
    }
    let f = v.as_f64()?;
    if f.fract() == 0.0 && (0.0..=MAX_SAFE as f64).contains(&f) {
        return Some(f as u64);
    }
    None
}

impl Lockfile {
    pub fn load(path: impl AsRef<Path>) -> Result<Lockfile> {
        let path = path.as_ref().to_path_buf();
        let shown = path.display().to_string();
        let empty = |path: PathBuf| Lockfile {
            path,
            entries: BTreeMap::new(),
            used: BTreeSet::new(),
            images: BTreeMap::new(),
            used_images: BTreeSet::new(),
            gits: BTreeMap::new(),
            used_gits: BTreeSet::new(),
            existed: false,
            original_text: None,
        };
        let raw = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(empty(path)),
            Err(e) => return Err(SitefileError::plain(E_VALIDATION, format!("cannot read lockfile {shown}: {e}"))),
        };
        let bad = |why: String| SitefileError::plain(E_VALIDATION, format!("malformed lockfile {shown}: {why}"));
        // fs.readFile(…, 'utf8') replaces invalid sequences; JSON.parse then sees them.
        let text = String::from_utf8_lossy(&raw).into_owned();
        let data: Value = serde_json::from_str(&text).map_err(|e| bad(e.to_string()))?;
        let obj = data.as_object().ok_or_else(|| bad("expected a JSON object".into()))?;
        let version = obj.get("version");
        if version.and_then(Value::as_f64) != Some(1.0) {
            let shown_v = version.map(|v| v.to_string()).unwrap_or_else(|| "undefined".into());
            return Err(bad(format!("unsupported version {shown_v} (expected 1)")));
        }
        let mut lock = empty(path.clone());
        let empty_obj = Map::new();
        let resources = match obj.get("resources") {
            None | Some(Value::Null) => &empty_obj,
            Some(Value::Object(m)) => m,
            Some(_) => return Err(bad("\"resources\" must be an object".into())),
        };
        for (url, raw) in resources {
            let e = raw.as_object().ok_or_else(|| bad(format!("entry for {url} must be an object")))?;
            let integrity = match e.get("integrity") {
                Some(Value::String(s)) if is_integrity(s) => s.clone(),
                _ => return Err(bad(format!("entry for {url} has an invalid \"integrity\" (expected sha256:<64 hex>)"))),
            };
            let size = e.get("size").and_then(safe_size).ok_or_else(|| bad(format!("entry for {url} has an invalid \"size\"")))?;
            let resolved = match e.get("resolved") {
                Some(Value::String(s)) => s.clone(),
                _ => return Err(bad(format!("entry for {url} has an invalid \"resolved\""))),
            };
            lock.entries.insert(url.clone(), LockEntry { integrity, size, resolved });
        }
        let images = match obj.get("images") {
            None | Some(Value::Null) => &empty_obj,
            Some(Value::Object(m)) => m,
            Some(_) => return Err(bad("\"images\" must be an object".into())),
        };
        for (r, raw) in images {
            match raw.get("digest") {
                Some(Value::String(d)) if raw.is_object() && is_integrity(d) => {
                    lock.images.insert(r.clone(), ImageLockEntry { digest: d.clone() });
                }
                _ => return Err(bad(format!("image entry for {r} needs a \"digest\" (sha256:<64 hex>)"))),
            }
        }
        let gits = match obj.get("git") {
            None | Some(Value::Null) => &empty_obj,
            Some(Value::Object(m)) => m,
            Some(_) => return Err(bad("\"git\" must be an object".into())),
        };
        for (url, raw) in gits {
            match raw.get("commit") {
                Some(Value::String(c)) if raw.is_object() && crate::resources::git::is_commit(c) => {
                    lock.gits.insert(url.clone(), GitLockEntry { commit: c.clone() });
                }
                _ => return Err(bad(format!("git entry for {url} needs a \"commit\" (40 lowercase hex digits)"))),
            }
        }
        lock.existed = true;
        lock.original_text = Some(text);
        Ok(lock)
    }

    /// The pinned digest for a canonical image reference (`docker.io/library/nginx:alpine`).
    pub fn get_image(&self, r: &str) -> Option<&ImageLockEntry> {
        self.images.get(r)
    }

    pub fn record_image(&mut self, r: &str, entry: ImageLockEntry) {
        self.images.insert(r.to_string(), entry);
        self.used_images.insert(r.to_string());
    }

    pub fn mark_image_used(&mut self, r: &str) {
        if self.images.contains_key(r) {
            self.used_images.insert(r.to_string());
        }
    }

    /// The commit a Git IMPORT URI was locked to.
    pub fn get_git(&self, url: &str) -> Option<&GitLockEntry> {
        self.gits.get(url)
    }

    pub fn record_git(&mut self, url: &str, entry: GitLockEntry) {
        self.gits.insert(url.to_string(), entry);
        self.used_gits.insert(url.to_string());
    }

    pub fn get(&self, url: &str) -> Option<&LockEntry> {
        self.entries.get(url)
    }

    /// Add or replace an entry and mark it used in this build.
    pub fn record(&mut self, url: &str, entry: LockEntry) {
        self.entries.insert(url.to_string(), entry);
        self.used.insert(url.to_string());
    }

    /// Keep an existing entry in the saved lockfile.
    pub fn mark_used(&mut self, url: &str) {
        if self.entries.contains_key(url) {
            self.used.insert(url.to_string());
        }
    }

    /// The JSON text `save()` would write.
    pub fn serialize(&self) -> String {
        let mut resources = Map::new();
        for url in &self.used {
            if let Some(e) = self.entries.get(url) {
                let mut m = Map::new();
                m.insert("integrity".into(), Value::String(e.integrity.clone()));
                m.insert("size".into(), Value::from(e.size));
                m.insert("resolved".into(), Value::String(e.resolved.clone()));
                resources.insert(url.clone(), Value::Object(m));
            }
        }
        let mut data = Map::new();
        data.insert("version".into(), Value::from(1));
        data.insert("resources".into(), Value::Object(resources));
        if !self.used_images.is_empty() {
            let mut images = Map::new();
            for r in &self.used_images {
                if let Some(e) = self.images.get(r) {
                    let mut m = Map::new();
                    m.insert("digest".into(), Value::String(e.digest.clone()));
                    images.insert(r.clone(), Value::Object(m));
                }
            }
            data.insert("images".into(), Value::Object(images));
        }
        if !self.used_gits.is_empty() {
            let mut gits = Map::new();
            for u in &self.used_gits {
                if let Some(e) = self.gits.get(u) {
                    let mut m = Map::new();
                    m.insert("commit".into(), Value::String(e.commit.clone()));
                    gits.insert(u.clone(), Value::Object(m));
                }
            }
            data.insert("git".into(), Value::Object(gits));
        }
        // JSON.stringify(data, null, 2) + '\n'
        let mut s = serde_json::to_string_pretty(&Value::Object(data)).unwrap_or_default();
        s.push('\n');
        s
    }

    /// Write the entries used in this build. Returns true if the file was
    /// written. Does nothing when the content is unchanged, and never creates
    /// a lockfile that would have no entries. The write is atomic (temp file + rename).
    pub fn save(&mut self) -> Result<bool> {
        let text = self.serialize();
        if !self.existed && self.used.is_empty() && self.used_images.is_empty() && self.used_gits.is_empty() {
            return Ok(false);
        }
        if self.existed && self.original_text.as_deref() == Some(text.as_str()) {
            return Ok(false);
        }
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        let mut tmp = self.path.clone().into_os_string();
        tmp.push(format!(".{}.{nanos}.tmp", std::process::id()));
        let tmp = PathBuf::from(tmp);
        let shown = self.path.display().to_string();
        let res = fs::write(&tmp, &text).and_then(|_| fs::rename(&tmp, &self.path));
        if let Err(e) = res {
            let _ = fs::remove_file(&tmp);
            return Err(SitefileError::plain(E_VALIDATION, format!("cannot write lockfile {shown}: {e}")));
        }
        self.existed = true;
        self.original_text = Some(text);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::local::tests::TempDir;

    fn h1() -> String {
        format!("sha256:{}", "a".repeat(64))
    }
    fn h2() -> String {
        format!("sha256:{}", "b".repeat(64))
    }

    #[test]
    fn missing_lockfile_loads_empty_and_is_not_created_when_unused() {
        let d = TempDir::new("sitefile-lock-");
        let path = d.0.join("Sitefile.lock");
        let mut lock = Lockfile::load(&path).unwrap();
        assert!(lock.get("https://x/").is_none());
        assert!(!lock.save().unwrap());
        assert!(!path.exists());
    }

    #[test]
    fn records_entries_with_sorted_keys_and_trailing_newline() {
        let d = TempDir::new("sitefile-lock-");
        let path = d.0.join("Sitefile.lock");
        let mut lock = Lockfile::load(&path).unwrap();
        lock.record("https://z.example/b", LockEntry { integrity: h2(), size: 2, resolved: "https://z.example/b".into() });
        lock.record("https://a.example/a", LockEntry { integrity: h1(), size: 1, resolved: "https://cdn.example/a".into() });
        assert!(lock.save().unwrap());
        let text = fs::read_to_string(&path).unwrap();
        let expected = format!(
            "{{\n  \"version\": 1,\n  \"resources\": {{\n    \"https://a.example/a\": {{\n      \"integrity\": \"{}\",\n      \"size\": 1,\n      \"resolved\": \"https://cdn.example/a\"\n    }},\n    \"https://z.example/b\": {{\n      \"integrity\": \"{}\",\n      \"size\": 2,\n      \"resolved\": \"https://z.example/b\"\n    }}\n  }}\n}}\n",
            h1(),
            h2()
        );
        assert_eq!(text, expected);
        assert!(!lock.save().unwrap(), "unchanged content is not rewritten");
    }

    #[test]
    fn empty_resources_serialize_like_json_stringify() {
        let d = TempDir::new("sitefile-lock-");
        let mut lock = Lockfile::load(d.0.join("Sitefile.lock")).unwrap();
        assert_eq!(lock.serialize(), "{\n  \"version\": 1,\n  \"resources\": {}\n}\n");
        lock.record_image("docker.io/library/nginx:alpine", ImageLockEntry { digest: h1() });
        assert!(lock.serialize().contains("\"images\": {\n    \"docker.io/library/nginx:alpine\": {\n      \"digest\""));
    }

    #[test]
    fn only_used_entries_are_kept_unchanged_files_not_rewritten() {
        let d = TempDir::new("sitefile-lock-");
        let path = d.0.join("Sitefile.lock");
        let mut first = Lockfile::load(&path).unwrap();
        first.record("https://a/", LockEntry { integrity: h1(), size: 1, resolved: "https://a/".into() });
        first.record("https://b/", LockEntry { integrity: h2(), size: 2, resolved: "https://b/".into() });
        first.save().unwrap();

        let mut second = Lockfile::load(&path).unwrap();
        assert_eq!(second.get("https://b/"), Some(&LockEntry { integrity: h2(), size: 2, resolved: "https://b/".into() }));
        second.mark_used("https://a/");
        second.mark_used("https://b/");
        second.mark_used("https://not-there/");
        assert!(!second.save().unwrap());

        let mut third = Lockfile::load(&path).unwrap();
        third.mark_used("https://a/");
        assert!(third.save().unwrap());
        let data: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let keys: Vec<&String> = data["resources"].as_object().unwrap().keys().collect();
        assert_eq!(keys, vec!["https://a/"]);
    }

    #[test]
    fn images_round_trip_and_unused_images_are_dropped() {
        let d = TempDir::new("sitefile-lock-");
        let path = d.0.join("Sitefile.lock");
        let mut first = Lockfile::load(&path).unwrap();
        first.record_image("docker.io/library/a:1", ImageLockEntry { digest: h1() });
        first.record_image("docker.io/library/b:1", ImageLockEntry { digest: h2() });
        assert!(first.save().unwrap());
        let mut second = Lockfile::load(&path).unwrap();
        assert_eq!(second.get_image("docker.io/library/b:1"), Some(&ImageLockEntry { digest: h2() }));
        second.mark_image_used("docker.io/library/a:1");
        second.mark_image_used("docker.io/library/missing:1");
        assert!(second.save().unwrap());
        let data: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(data["images"].as_object().unwrap().keys().collect::<Vec<_>>(), vec!["docker.io/library/a:1"]);
    }

    #[test]
    fn malformed_lockfiles_fail_with_e_validation() {
        let d = TempDir::new("sitefile-lock-");
        let path = d.0.join("Sitefile.lock");
        let h = h1();
        let cases = vec![
            "{not json".to_string(),
            "[]".into(),
            r#"{"version":2,"resources":{}}"#.into(),
            r#"{"version":1,"resources":[]}"#.into(),
            r#"{"version":1,"resources":{"https://a/":{"integrity":"md5:x","size":1,"resolved":"https://a/"}}}"#.into(),
            format!(r#"{{"version":1,"resources":{{"https://a/":{{"integrity":"{h}","size":-1,"resolved":"https://a/"}}}}}}"#),
            format!(r#"{{"version":1,"resources":{{"https://a/":{{"integrity":"{h}","size":1}}}}}}"#),
            format!(r#"{{"version":1,"resources":{{"https://a/":{{"integrity":"{h}","size":1.5,"resolved":"x"}}}}}}"#),
            r#"{"version":1,"resources":{},"images":[]}"#.into(),
            r#"{"version":1,"resources":{},"images":{"x":{"digest":"nope"}}}"#.into(),
        ];
        for c in cases {
            fs::write(&path, &c).unwrap();
            let e = Lockfile::load(&path).unwrap_err();
            assert_eq!(e.code, E_VALIDATION, "{c}");
            assert!(e.message().starts_with("malformed lockfile "), "{}", e.message());
        }
    }
}

//! `sitefile outdated` (port of `src/outdated.ts`, issue #104): report, for every remote input, whether
//! what it points at today differs from its pin. It never writes anything (no lockfile, no image
//! store); `--update-lock` stays the way to change pins.
//!
//! What is checkable, honestly:
//! - an https IMPORT has no notion of "a newer version": the URL is fetched again (a full GET under the
//!   build's network policy) and the body's sha256 is compared with the pin (INTEGRITY, else the
//!   Sitefile.lock entry). Sitefile.lock records no ETag or Last-Modified, so there is nothing to make a
//!   conditional request against, and a changed validator would not prove changed content anyway. A
//!   version inside a URL (`.../lib@1.2.3/x.js`) is not interpreted.
//! - a BASE tag (with or without `@sha256:`) is resolved again (one manifest GET) and compared with the
//!   digest pinned in the Sitefile or recorded in Sitefile.lock: "changed" means the tag has moved. A
//!   digest-only reference has no tag to follow.
//! - for a tag that looks like a version (`[v]N[.N[.N]][-suffix]`), the registry's tag list is read and
//!   the highest tag of the same shape (same `v`, number of components and suffix) that is greater is
//!   reported as `newerTag`. Registries that refuse to list tags just give no `newerTag`.

use crate::digest::sha256;
use crate::error::Result;
use crate::executor::{
    load_program, read_imported_sitefile, root_sitefile_source, BuildOptions, Loaded,
    LockfileSetting, SitefileSource,
};
use crate::language::ast::{ExportFormat, ImportKind};
use crate::planner::{imported_stage_refs, plan, ImportLocation, Plan, StageBase};
use crate::refs::{parse_ref, with_default_registry};
use crate::registry::{insecure_registries_from_env, make_client, process_env, RegistryOptions};
use crate::resources::https::{fetch_remote_with, FetchRequest};
use crate::resources::lockfile::Lockfile;
use crate::secrets::{redact_secrets, secret_headers};
use serde_json::{json, Map, Value};
use std::cmp::Ordering;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Current,
    Changed,
    Unpinned,
    DigestOnly,
    Error,
}

impl Status {
    pub const ALL: [Status; 5] = [
        Status::Current,
        Status::Changed,
        Status::Unpinned,
        Status::DigestOnly,
        Status::Error,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Current => "current",
            Status::Changed => "changed",
            Status::Unpinned => "unpinned",
            Status::DigestOnly => "digest-only",
            Status::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// `import` or `base`.
    pub kind: &'static str,
    /// The IMPORT name, the stage of a `FROM image` base (its name, or `#<n>`), or `BASE`.
    pub name: String,
    /// The URL, or the canonical image reference.
    pub source: String,
    pub pin: Option<String>,
    /// `integrity`, `lockfile` or `digest`.
    pub pinned_by: Option<&'static str>,
    /// What the source resolves to now.
    pub latest: Option<String>,
    pub status: Status,
    /// Images only (BASE and FROM image): a greater version-like tag of the same shape.
    pub newer_tag: Option<String>,
    /// (code, message)
    pub error: Option<(String, String)>,
}

pub struct Report {
    pub inputs: Vec<Entry>,
}

impl Report {
    pub fn count(&self, s: Status) -> usize {
        self.inputs.iter().filter(|i| i.status == s).count()
    }

    /// The `--json` document: `{inputs: [...], summary: {...}}`, keys in the documented order.
    pub fn to_json(&self) -> Value {
        let opt = |v: &Option<String>| v.clone().map(Value::String).unwrap_or(Value::Null);
        let inputs: Vec<Value> = self
            .inputs
            .iter()
            .map(|i| {
                let mut m = Map::new();
                m.insert("kind".into(), json!(i.kind));
                m.insert("name".into(), json!(i.name));
                m.insert("source".into(), json!(i.source));
                m.insert("pin".into(), opt(&i.pin));
                m.insert(
                    "pinnedBy".into(),
                    i.pinned_by.map(|s| json!(s)).unwrap_or(Value::Null),
                );
                m.insert("latest".into(), opt(&i.latest));
                m.insert("status".into(), json!(i.status.as_str()));
                m.insert("newerTag".into(), opt(&i.newer_tag));
                m.insert(
                    "error".into(),
                    i.error
                        .as_ref()
                        .map(|(c, msg)| json!({ "code": c, "message": msg }))
                        .unwrap_or(Value::Null),
                );
                Value::Object(m)
            })
            .collect();
        let mut summary = Map::new();
        for s in Status::ALL {
            summary.insert(s.as_str().into(), json!(self.count(s)));
        }
        json!({ "inputs": inputs, "summary": summary })
    }
}

struct VersionTag {
    prefix: bool,
    parts: Vec<String>,
    suffix: String,
}

/// `[v]N[.N[.N]][-suffix]` where suffix is `-[0-9A-Za-z][0-9A-Za-z._-]*`.
fn version_tag(tag: &str) -> Option<VersionTag> {
    let (prefix, rest) = match tag.strip_prefix('v') {
        Some(r) => (true, r),
        None => (false, tag),
    };
    let (nums, suffix) = match rest.find('-') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if !suffix.is_empty() {
        let body = &suffix[1..];
        let mut cs = body.chars();
        if !cs.next().is_some_and(|c| c.is_ascii_alphanumeric())
            || !cs.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            return None;
        }
    }
    let parts: Vec<&str> = nums.split('.').collect();
    if parts.is_empty()
        || parts.len() > 3
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let parts = parts
        .iter()
        .map(|p| {
            let t = p.trim_start_matches('0');
            if t.is_empty() {
                "0".to_string()
            } else {
                t.to_string()
            }
        })
        .collect();
    Some(VersionTag {
        prefix,
        parts,
        suffix: suffix.to_string(),
    })
}

fn cmp_num(a: &str, b: &str) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

fn cmp_version(a: &VersionTag, b: &VersionTag) -> Ordering {
    for (x, y) in a.parts.iter().zip(b.parts.iter()) {
        let c = cmp_num(x, y);
        if c != Ordering::Equal {
            return c;
        }
    }
    Ordering::Equal
}

/// The greatest tag of `tags` with the same shape as `current` that is greater than it (ties: the first in byte order).
pub fn newer_version_tag(current: &str, tags: &[String]) -> Option<String> {
    let cur = version_tag(current)?;
    let mut sorted: Vec<&String> = tags.iter().collect();
    sorted.sort();
    let mut best: Option<(&String, VersionTag)> = None;
    for t in sorted {
        let Some(v) = version_tag(t) else { continue };
        if v.prefix != cur.prefix || v.suffix != cur.suffix || v.parts.len() != cur.parts.len() {
            continue;
        }
        if cmp_version(&v, &cur) != Ordering::Greater {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|(_, b)| cmp_version(&v, b) == Ordering::Greater)
        {
            best = Some((t, v));
        }
    }
    best.map(|(t, _)| t.clone())
}

fn status_of(pin: Option<&str>, latest: &str) -> Status {
    match pin {
        None => Status::Unpinned,
        Some(p) if p == latest => Status::Current,
        Some(_) => Status::Changed,
    }
}

pub fn outdated(options: &BuildOptions) -> Result<Report> {
    let Loaded {
        program,
        sitefile_path,
        label,
        ..
    } = load_program(
        &options.context_dir,
        options.sitefile.as_deref(),
        options.label.as_deref(),
    )?;
    let p = plan(&program, &options.build_args)?;
    if let Some(w) = &options.on_warning {
        for m in &p.warnings {
            w(m);
        }
    }
    let lock = match &options.lockfile {
        LockfileSetting::Disabled => None,
        LockfileSetting::Path(p) => Some(Lockfile::load(p)?),
        LockfileSetting::Default => Some(Lockfile::load(
            sitefile_path
                .parent()
                .unwrap_or(Path::new("/"))
                .join("Sitefile.lock"),
        )?),
    };
    let mut inputs = Vec::new();
    // Sitefiles imported AS sitefile, with the prefix their inputs are named by (`theme+`).
    let mut nested: Vec<(String, Plan)> = Vec::new();
    let root = root_sitefile_source(options, &label, &sitefile_path)?;
    import_rows(
        &p,
        &root,
        "",
        options,
        lock.as_ref(),
        &mut inputs,
        &mut nested,
    );

    // A registry image (BASE or a FROM image base): resolve its tag again and compare with the pin.
    image_rows(&p, &nested, options, lock.as_ref(), &mut inputs);
    Ok(Report { inputs })
}

/// The rows of `sp`'s imports (`sp` read from `source`), then of the Sitefiles it imports, named `<prefix><name>`.
fn import_rows(
    sp: &Plan,
    source: &SitefileSource,
    prefix: &str,
    options: &BuildOptions,
    lock: Option<&Lockfile>,
    inputs: &mut Vec<Entry>,
    nested: &mut Vec<(String, Plan)>,
) {
    for spec in &sp.imports {
        let name = format!("{prefix}{}", spec.name);
        let row_index = inputs.len();
        let mut has_row = false;
        if let ImportLocation::Git(loc) = &spec.location {
            has_row = true;
            // A ref is resolved again (ls-refs, nothing is downloaded): changed = the ref moved since it
            // was locked. A commit in the URL has no ref to follow.
            let locked = lock
                .as_ref()
                .and_then(|l| l.get_git(&loc.url))
                .map(|e| e.commit.clone());
            let pinned_by = if loc.commit.is_some() {
                Some("commit")
            } else if locked.is_some() {
                Some("lockfile")
            } else {
                None
            };
            let pin = loc.commit.clone().or(locked);
            let mut e = Entry {
                kind: "import",
                name: name.clone(),
                source: loc.url.clone(),
                pin,
                pinned_by,
                latest: None,
                status: Status::Error,
                newer_tag: None,
                error: None,
            };
            if loc.commit.is_some() {
                e.status = Status::DigestOnly;
            } else {
                let r = secret_headers(&spec.name, &spec.secrets, &spec.span, &options.secrets)
                    .and_then(|origin_headers| {
                        let t = crate::resources::git::GitTransport {
                            origin_headers: &origin_headers,
                            ..crate::resources::git::GitTransport::new(
                                &options.fetch_policy,
                                Some(&spec.span),
                            )
                        };
                        crate::resources::git::resolve_ref(&loc.repo, &loc.git_ref, t)
                    });
                match r {
                    Ok(latest) => {
                        e.status = status_of(e.pin.as_deref(), &latest);
                        e.latest = Some(latest);
                    }
                    Err(err) => {
                        e.error = Some((
                            err.code.as_str().to_string(),
                            redact_secrets(&err.detail, &options.secrets),
                        ))
                    }
                }
            }
            inputs.push(e);
        } else if let ImportLocation::Remote { url } = &spec.location {
            remote_row(spec, url, &name, options, lock, inputs);
        }
        if spec.as_kind != Some(ImportKind::Sitefile)
            || (has_row && inputs[row_index].status == Status::Error)
        {
            continue;
        }
        // The inputs of an imported Sitefile, as pinned (a Git one is read at its pinned commit, else the ref's).
        let limits = options.archive_limits;
        let read = read_imported_sitefile(
            spec,
            source,
            &imported_stage_refs(&sp.stages),
            &mut |s, loc| {
                let origin_headers =
                    secret_headers(&s.name, &s.secrets, &s.span, &options.secrets)?;
                let want = loc.commit.clone().or_else(|| {
                    lock.and_then(|l| l.get_git(&loc.url))
                        .map(|e| e.commit.clone())
                });
                let t = crate::resources::git::GitTransport {
                    origin_headers: &origin_headers,
                    ..crate::resources::git::GitTransport::new(&options.fetch_policy, Some(&s.span))
                };
                let objs = crate::resources::git::fetch_git_commit(
                    &loc.repo,
                    &loc.git_ref,
                    want.as_deref(),
                    t,
                    &limits,
                )?;
                let selection = crate::resources::git::select_git_path(
                    &objs,
                    &loc.repo,
                    &loc.path,
                    &limits,
                    Some(&s.span),
                )?;
                Ok((objs.commit.clone(), selection))
            },
        );
        match read {
            Ok((sub, sub_source)) => {
                if let Some(w) = &options.on_warning {
                    for m in &sub.warnings {
                        w(m);
                    }
                }
                let sub_prefix = format!("{name}+");
                nested.push((sub_prefix.clone(), sub.clone()));
                import_rows(
                    &sub,
                    &sub_source,
                    &sub_prefix,
                    options,
                    lock,
                    inputs,
                    nested,
                );
            }
            Err(err) => {
                let error = Some((
                    err.code.as_str().to_string(),
                    redact_secrets(&err.detail, &options.secrets),
                ));
                if has_row {
                    let e = &mut inputs[row_index];
                    e.latest = None;
                    e.status = Status::Error;
                    e.error = error;
                } else {
                    inputs.push(Entry {
                        kind: "import",
                        name,
                        source: spec.uri.clone(),
                        pin: None,
                        pinned_by: None,
                        latest: None,
                        status: Status::Error,
                        newer_tag: None,
                        error,
                    });
                }
            }
        }
    }
}

/// The row of an https IMPORT: the URL fetched again and its digest compared with the pin.
fn remote_row(
    spec: &crate::planner::ImportSpec,
    url: &str,
    name: &str,
    options: &BuildOptions,
    lock: Option<&Lockfile>,
    inputs: &mut Vec<Entry>,
) {
    {
        let locked = lock.and_then(|l| l.get(url));
        let pin = spec
            .integrity
            .clone()
            .or_else(|| locked.map(|e| e.integrity.clone()));
        let pinned_by = if spec.integrity.is_some() {
            Some("integrity")
        } else if locked.is_some() {
            Some("lockfile")
        } else {
            None
        };
        let mut e = Entry {
            kind: "import",
            name: name.to_string(),
            source: url.to_string(),
            pin,
            pinned_by,
            latest: None,
            status: Status::Error,
            newer_tag: None,
            error: None,
        };
        let r = secret_headers(&spec.name, &spec.secrets, &spec.span, &options.secrets).and_then(
            |origin_headers| {
                fetch_remote_with(
                    url,
                    &options.fetch_policy,
                    None,
                    &FetchRequest {
                        origin_headers,
                        ..FetchRequest::default()
                    },
                )
            },
        );
        match r {
            Ok(f) => {
                let latest = sha256(&f.bytes);
                e.status = status_of(e.pin.as_deref(), &latest);
                e.latest = Some(latest);
            }
            Err(err) => {
                e.error = Some((
                    err.code.as_str().to_string(),
                    redact_secrets(&err.detail, &options.secrets),
                ))
            }
        }
        inputs.push(e);
    }
}

/// The rows of the registry FROM images (this Sitefile's, then the imported ones', named `<prefix><stage>`) and the BASE.
fn image_rows(
    p: &Plan,
    nested: &[(String, Plan)],
    options: &BuildOptions,
    lock: Option<&Lockfile>,
    inputs: &mut Vec<Entry>,
) {
    let image_row = |kind: &'static str, name: String, input: &str| -> Entry {
        let mut e = Entry {
            kind,
            name,
            source: input.to_string(),
            pin: None,
            pinned_by: None,
            latest: None,
            status: Status::Error,
            newer_tag: None,
            error: None,
        };
        let r = (|| -> Result<()> {
            let r = with_default_registry(&parse_ref(input)?);
            let registry = r.registry.clone().unwrap_or_default();
            e.source = format!(
                "{registry}/{}{}{}",
                r.name,
                r.tag.as_ref().map(|t| format!(":{t}")).unwrap_or_default(),
                r.digest
                    .as_ref()
                    .map(|d| format!("@{d}"))
                    .unwrap_or_default()
            );
            let tag = r.tag.clone().unwrap_or_else(|| "latest".into());
            let locked = lock
                .as_ref()
                .and_then(|l| l.get_image(&format!("{registry}/{}:{tag}", r.name)))
                .map(|l| l.digest.clone());
            e.pin = r.digest.clone().or(locked);
            e.pinned_by = if r.digest.is_some() {
                Some("digest")
            } else if e.pin.is_some() {
                Some("lockfile")
            } else {
                None
            };
            if r.tag.is_none() && r.digest.is_some() {
                e.status = Status::DigestOnly;
                return Ok(());
            }
            let env = process_env();
            let o = RegistryOptions {
                insecure_registries: insecure_registries_from_env(
                    &options.insecure_registries,
                    &env,
                ),
                allow_private_network: options.fetch_policy.allow_private_network,
                ..Default::default()
            };
            let client = make_client(&registry, &o, None, &env);
            let latest = client.get_manifest(&r.name, &tag)?.digest;
            if version_tag(&tag).is_some() {
                // not listable: no newer tag
                if let Ok(tags) = client.list_tags(&r.name) {
                    e.newer_tag = newer_version_tag(&tag, &tags);
                }
            }
            e.status = status_of(e.pin.as_deref(), &latest);
            e.latest = Some(latest);
            Ok(())
        })();
        if let Err(err) = r {
            e.status = Status::Error;
            e.error = Some((err.code.as_str().to_string(), err.detail));
        }
        e
    };
    // FROM image stage bases with a registry (local-store images have no upstream), named by their stage.
    for (prefix, sp) in
        std::iter::once(("", p)).chain(nested.iter().map(|(pre, sp)| (pre.as_str(), sp)))
    {
        for (i, st) in sp.stages.iter().enumerate() {
            if let StageBase::Image {
                reference,
                local: false,
                ..
            } = &st.base
            {
                inputs.push(image_row(
                    "image",
                    format!(
                        "{prefix}{}",
                        st.name.clone().unwrap_or_else(|| format!("#{}", i + 1))
                    ),
                    reference,
                ));
            }
        }
    }
    let base_input = options.base_image.clone().or_else(|| {
        if p.export_format == ExportFormat::Oci {
            p.export_base.clone()
        } else {
            None
        }
    });
    if let Some(input) = base_input {
        inputs.push(image_row("base", "BASE".into(), &input));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(t: &[&str]) -> Vec<String> {
        t.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn base_tags_moved_pinned_digest_only_and_newer() {
        use crate::registry::mock::*;
        use crate::registry::RegistryClient;
        let reg = MockRegistry::start(MockOptions {
            tag_page_size: Some(2),
            ..Default::default()
        });
        let img = |n: &'static str| SeedImage {
            layers: vec![vec![("/srv/v.txt", n)]],
            config: json!({ "Cmd": [n] }),
            ..Default::default()
        };
        let v1 = reg.seed_image("lib/base", "1.0.0", img("1"));
        reg.seed_image("lib/base", "1.1.0", img("2"));
        reg.seed_image("lib/base", "1.10.0", img("3"));
        reg.seed_image("lib/base", "1.2.0-alpine", img("4"));
        reg.seed_image("lib/base", "latest", img("5"));
        let ctx = tmp("ctx-");
        let reference = format!("{}/lib/base:1.0.0", reg.host);
        std::fs::write(
            ctx.join("Sitefile"),
            format!("FROM scratch\nEXPORT / AS oci BASE {reference}\n"),
        )
        .unwrap();
        let o = BuildOptions {
            context_dir: ctx.display().to_string(),
            ..Default::default()
        };
        let r = outdated(&o).unwrap();
        let e = &r.inputs[0];
        assert_eq!(
            (
                e.kind,
                e.status,
                e.newer_tag.as_deref(),
                e.latest.as_deref()
            ),
            (
                "base",
                Status::Unpinned,
                Some("1.10.0"),
                Some(v1.manifest_digest.as_str())
            )
        );
        let lock = format!(
            r#"{{"version":1,"resources":{{}},"images":{{"{reference}":{{"digest":"{}"}}}}}}"#,
            v1.manifest_digest
        );
        std::fs::write(ctx.join("Sitefile.lock"), &lock).unwrap();
        assert_eq!(outdated(&o).unwrap().inputs[0].status, Status::Current);
        let moved = reg.seed_image("lib/base", "1.0.0", img("moved"));
        let e = outdated(&o).unwrap().inputs.remove(0);
        assert_eq!(
            (e.status, e.pinned_by, e.pin.as_deref(), e.latest.as_deref()),
            (
                Status::Changed,
                Some("lockfile"),
                Some(v1.manifest_digest.as_str()),
                Some(moved.manifest_digest.as_str())
            )
        );
        assert_eq!(
            std::fs::read_to_string(ctx.join("Sitefile.lock")).unwrap(),
            lock,
            "never modified"
        );
        // a tag with a digest pin, a digest-only reference (no request), a missing repository
        let pinned = BuildOptions {
            base_image: Some(format!("{reference}@{}", v1.manifest_digest)),
            lockfile: LockfileSetting::Disabled,
            ..o.clone()
        };
        let e = outdated(&pinned).unwrap().inputs.remove(0);
        assert_eq!((e.status, e.pinned_by), (Status::Changed, Some("digest")));
        let n = reg.requests().len();
        let only = BuildOptions {
            base_image: Some(format!("{}/lib/base@{}", reg.host, v1.manifest_digest)),
            ..o.clone()
        };
        assert_eq!(
            outdated(&only).unwrap().inputs[0].status,
            Status::DigestOnly
        );
        assert_eq!(reg.requests().len(), n);
        let missing = BuildOptions {
            base_image: Some(format!("{}/lib/nope:1", reg.host)),
            ..o.clone()
        };
        let e = outdated(&missing).unwrap().inputs.remove(0);
        assert_eq!(
            (e.status, e.error.map(|x| x.0)),
            (Status::Error, Some("E_NOT_FOUND".to_string()))
        );
        // pagination follows Link headers
        let client = RegistryClient::new(&reg.host, RegistryOptions::default());
        assert_eq!(
            client.list_tags("lib/base").unwrap(),
            ["1.0.0", "1.1.0", "1.10.0", "1.2.0-alpine", "latest"]
        );
    }

    #[test]
    fn version_like_tags() {
        let all = tags(&[
            "1.25-alpine",
            "1.27-alpine",
            "1.26-alpine",
            "1.27.1-alpine",
            "1.30",
            "v1.40-alpine",
            "latest",
            "1.9-alpine",
            "1.100-alpine-slim",
        ]);
        assert_eq!(
            newer_version_tag("1.25-alpine", &all).as_deref(),
            Some("1.27-alpine")
        );
        assert_eq!(newer_version_tag("1.27-alpine", &all), None);
        assert_eq!(
            newer_version_tag("1.9", &tags(&["1.10", "1.09", "1.11", "2", "x"])).as_deref(),
            Some("1.11")
        );
        assert_eq!(
            newer_version_tag("v1", &tags(&["v2", "v10", "3"])).as_deref(),
            Some("v10")
        );
        assert_eq!(
            newer_version_tag("1.2.3", &tags(&["1.2.4", "1.3.0", "01.3.1"])).as_deref(),
            Some("01.3.1")
        );
        assert_eq!(newer_version_tag("latest", &all), None);
        assert_eq!(newer_version_tag("1.0", &tags(&["1.00", "1.0"])), None);
        assert!(version_tag("1.2.3.4").is_none());
        assert!(version_tag("1.2-").is_none());
        assert!(version_tag("-1").is_none());
        assert!(version_tag("v").is_none());
    }
}

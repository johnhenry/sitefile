//! `EXPORT ... AS oci BASE <image>`: resolve a base image into the local store
//! (port of `src/base-image.ts`).
//!
//! - `image@sha256:...` is pinned by the Sitefile itself and used as is;
//! - `image:tag` is looked up in Sitefile.lock (`images`) and fetched BY
//!   DIGEST, so a tag that has moved on the registry does not change the build;
//!   without a lock entry the tag is resolved once and recorded (an error with
//!   `--frozen-lockfile`; re-resolved with `--update-lock`);
//! - a base already in the store (matching manifest and blobs) is used without
//!   any network access.
//!
//! A bare name (`nginx:alpine`) means Docker Hub (`docker.io/library/nginx`).

use crate::error::{ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::export::oci::{BaseImage, Descriptor};
use crate::refs::{parse_ref, with_default_registry, ImageRef};
use crate::registry::{make_client, process_env, pull_image, resolve_digest, Env, PullImageOptions, RegistryOptions};
use crate::resources::lockfile::{ImageLockEntry, Lockfile};
use crate::store::Store;

pub use crate::registry::insecure_registries_from_env;

pub struct ResolveBaseOptions<'a> {
    pub store: &'a Store,
    /// `os/arch` of the base to use (default linux/amd64).
    pub platform: Option<String>,
    /// `--frozen-lockfile`: never resolve a tag; it must be in the lockfile (or pinned by digest).
    pub frozen: bool,
    /// `--update-lock`: re-resolve the tag and re-record it.
    pub update: bool,
    pub registry: RegistryOptions,
    /// Environment for credentials and `$SITEFILE_INSECURE_REGISTRIES` (default: the process environment).
    pub env: Option<Env>,
    pub warn: Option<&'a dyn Fn(&str)>,
    /// Where the BASE was written (for error positions).
    pub span: Option<SourceSpan>,
}

impl<'a> ResolveBaseOptions<'a> {
    pub fn new(store: &'a Store) -> Self {
        ResolveBaseOptions { store, platform: None, frozen: false, update: false, registry: RegistryOptions::default(), env: None, warn: None, span: None }
    }
}

/// Resolve `input` to a base image whose blobs are in the local store. `lock`
/// is the build's lockfile (`None` with `--no-lockfile`); a fresh tag resolution
/// is recorded in it (the caller saves it).
/// A parsed BASE reference (a bare name means Docker Hub), how it is shown, and its `Sitefile.lock` `images` key.
pub struct BaseRef {
    pub r: ImageRef,
    pub registry: String,
    pub display: String,
    pub lock_key: String,
}

pub fn base_ref(input: &str, span: Option<&SourceSpan>) -> Result<BaseRef> {
    let r = parse_ref(input).map(|r| with_default_registry(&r)).map_err(|e| SitefileError::new(e.code, format!("BASE: {}", e.detail), span.cloned()))?;
    let registry = r.registry.clone().unwrap_or_default();
    let tag = r.tag.clone().unwrap_or_else(|| "latest".into());
    let display = if r.tag.is_some() || r.digest.is_none() {
        format!("{registry}/{}:{tag}", r.name)
    } else {
        format!("{registry}/{}", r.name)
    };
    let lock_key = format!("{registry}/{}:{tag}", r.name);
    Ok(BaseRef { r, registry, display, lock_key })
}

pub fn resolve_base(input: &str, lock: Option<&mut Lockfile>, o: &ResolveBaseOptions<'_>) -> Result<BaseImage> {
    let BaseRef { r, registry, display, lock_key } = base_ref(input, o.span.as_ref())?;
    let env = o.env.clone().unwrap_or_else(process_env);
    let options = RegistryOptions {
        insecure_registries: insecure_registries_from_env(&o.registry.insecure_registries, &env),
        ..o.registry.clone()
    };
    let client = make_client(&registry, &options, None, &env);

    let mut root_digest = r.digest.clone();
    let mut fresh = false;
    if root_digest.is_none() {
        let mut lock = lock;
        let locked = if o.update { None } else { lock.as_deref().and_then(|l| l.get_image(&lock_key)).map(|e| e.digest.clone()) };
        if let Some(d) = locked {
            root_digest = Some(d);
            if let Some(l) = lock.as_deref_mut() {
                l.mark_image_used(&lock_key);
            }
        } else if o.frozen {
            return Err(SitefileError::new(
                E_LOCK_MISSING,
                format!(
                    "BASE {input}: '{lock_key}' is not in the lockfile (--frozen-lockfile); pin it with @sha256:... or run a build without --frozen-lockfile to record it"
                ),
                o.span.clone(),
            ));
        } else {
            let d = resolve_digest(o.store, &client, &r)?;
            fresh = true;
            match lock {
                Some(l) => l.record_image(&lock_key, ImageLockEntry { digest: d.clone() }),
                None => {
                    if let Some(w) = o.warn {
                        w(&format!(
                            "BASE {input} is not pinned and there is no lockfile: the build is not reproducible (use {display}@sha256:... or a Sitefile.lock)"
                        ));
                    }
                }
            }
            root_digest = Some(d);
        }
    }
    let pulled = pull_image(
        o.store,
        &client,
        &r,
        &PullImageOptions { platform: o.platform.clone(), enforce_platform: true, root_digest, tag: fresh, on_event: None },
    )?;
    Ok(BaseImage { name: display, manifest_digest: pulled.id, layers: pulled.manifest.layers, config: pulled.config })
}

/// Read the blobs of `layers` from the store (verifying each digest), for
/// writing a self-contained layout (`layout_tree` / `oci_archive` `base_blobs`).
pub fn read_layer_blobs(store: &Store, layers: &[Descriptor]) -> Result<Vec<(String, Vec<u8>)>> {
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    for l in layers {
        if !out.iter().any(|(d, _)| *d == l.digest) {
            out.push((l.digest.clone(), store.read_blob(&l.digest, Some(l.size))?));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest::sha256;
    use crate::export::oci::{ann, build_oci_image, media, parse_config, parse_manifest, OciOptions};
    use crate::refs::parse_local_ref;
    use crate::registry::mock::*;
    use crate::registry::{push, PushOptions};
    use crate::tree::{make_file, Tree};
    use serde_json::json;
    use std::cell::RefCell;

    fn seed_base(reg: &MockRegistry, tag: &str) -> Seeded {
        reg.seed_image(
            "lib/base",
            tag,
            SeedImage {
                layers: vec![
                    vec![("/usr/share/base.txt", "base")],
                    vec![("/usr/share/nginx/html/index.html", "default page"), ("/usr/share/nginx/html/50x.html", "oops")],
                ],
                config: json!({
                    "Entrypoint": ["/docker-entrypoint.sh"],
                    "Cmd": ["nginx", "-g", "daemon off;"],
                    "Env": ["PATH=/usr/local/sbin:/usr/bin", "NGINX_VERSION=1.2.3"],
                    "ExposedPorts": { "80/tcp": {} },
                    "WorkingDir": "/srv",
                    "Labels": { "maintainer": "base-maintainer", "org.opencontainers.image.title": "base-title" },
                }),
                docker: true,
                ..Default::default()
            },
        )
    }

    fn site() -> Tree {
        let files = [("/index.html", "<h1>Home</h1>"), ("/style.css", "body{}")];
        Tree::from_files(files.iter().map(|(p, c)| (p.to_string(), make_file(c.as_bytes().to_vec(), "test", 0o644)))).unwrap()
    }

    fn opts(store: &Store) -> ResolveBaseOptions<'_> {
        ResolveBaseOptions { env: Some(clean_env()), ..ResolveBaseOptions::new(store) }
    }

    fn image_on(base: BaseImage, epoch: Option<u64>) -> crate::export::oci::OciImage {
        let o = OciOptions { root: Some("/usr/share/nginx/html".into()), base: Some(base), epoch, labels: [("maintainer".to_string(), "me".to_string())].into(), ..Default::default() };
        build_oci_image(&site(), &o).unwrap()
    }

    #[test]
    fn base_layers_config_and_labels() {
        let reg = MockRegistry::start(MockOptions::default());
        let b = seed_base(&reg, "1");
        let store = Store::open(tmp("home-")).unwrap();
        let input = format!("{}/lib/base:1", reg.host);
        let base = resolve_base(&input, None, &opts(&store)).unwrap();
        assert_eq!(base.name, input);
        assert_eq!(base.manifest_digest, b.manifest_digest);
        let img = image_on(base.clone(), None);
        let m = parse_manifest(&img.manifest_bytes).unwrap();
        assert_eq!(m.layers.len(), 3);
        assert_eq!(m.layers[..2].iter().map(|l| l.digest.clone()).collect::<Vec<_>>(), b.layer_digests);
        assert!(m.layers.iter().all(|l| l.media_type == media::LAYER), "Docker layer types are normalized to OCI");
        assert_eq!(m.annotations[ann::BASE_DIGEST], b.manifest_digest);
        assert_eq!(m.annotations[ann::BASE_NAME], input);
        let cfg = parse_config(&img.config_bytes).unwrap();
        assert_eq!(cfg.diff_ids[..2], b.diff_ids[..]);
        assert_eq!(cfg.created, "2024-01-02T03:04:05Z", "created is the base's unless SOURCE_DATE_EPOCH is set");
        assert_eq!(cfg.labels["maintainer"], "me");
        assert_eq!(cfg.labels[ann::ROOT], "/usr/share/nginx/html");
        let c: serde_json::Value = serde_json::from_slice(&img.config_bytes).unwrap();
        assert_eq!(c["config"]["Cmd"], json!(["nginx", "-g", "daemon off;"]));
        assert_eq!(c["config"]["WorkingDir"], "/srv");
        // every base blob is in the store and readable for a self-contained layout
        let blobs = read_layer_blobs(&store, &img.base_layers).unwrap();
        assert_eq!(blobs.len(), 2);
        for (d, bytes) in &blobs {
            assert_eq!(&sha256(bytes), d);
            assert!(reg.has_blob(d));
        }
    }

    #[test]
    fn pinned_base_is_reproducible_and_needs_no_lockfile() {
        let reg = MockRegistry::start(MockOptions::default());
        let b = seed_base(&reg, "1");
        let input = format!("{}/lib/base:1@{}", reg.host, b.manifest_digest);
        let mut digests = Vec::new();
        for _ in 0..2 {
            let store = Store::open(tmp("home-")).unwrap();
            let base = resolve_base(&input, None, &ResolveBaseOptions { frozen: true, ..opts(&store) }).unwrap();
            digests.push(image_on(base, Some(1_700_000_000)).manifest_digest);
        }
        assert_eq!(digests[0], digests[1]);
    }

    #[test]
    fn tag_resolution_is_recorded_and_frozen_builds_never_reresolve() {
        let reg = MockRegistry::start(MockOptions::default());
        let v1 = seed_base(&reg, "1");
        let input = format!("{}/lib/base:1", reg.host);
        let key = input.clone();
        let lock_path = tmp("ctx-").join("Sitefile.lock");

        // frozen with no lock entry: error, and nothing was pulled
        let mut lock = Lockfile::load(&lock_path).unwrap();
        let s0 = Store::open(tmp("home-")).unwrap();
        let n0 = reg.requests().len();
        let e = resolve_base(&input, Some(&mut lock), &ResolveBaseOptions { frozen: true, ..opts(&s0) }).unwrap_err();
        assert_eq!(e.code, E_LOCK_MISSING);
        assert!(e.message().contains("not in the lockfile"));
        assert_eq!(reg.requests().len(), n0);

        let s1 = Store::open(tmp("home-")).unwrap();
        let b1 = resolve_base(&input, Some(&mut lock), &opts(&s1)).unwrap();
        assert_eq!(lock.get_image(&key).unwrap().digest, v1.manifest_digest);
        assert!(lock.save().unwrap());
        assert_eq!(s1.resolve(&input).unwrap(), v1.manifest_digest, "a fresh resolution tags the base locally");

        // the tag moves on the registry...
        let v2 = reg.seed_image("lib/base", "1", SeedImage { layers: vec![vec![("/x", "new")]], config: json!({"Cmd": ["new"]}), docker: true, ..Default::default() });
        assert_ne!(v2.manifest_digest, v1.manifest_digest);
        // ...a locked (even frozen) build still uses the locked digest, from a cold cache
        let tag_calls = || reg.requests().iter().filter(|r| r.url == "/v2/lib/base/manifests/1").count();
        let before = tag_calls();
        let mut lock = Lockfile::load(&lock_path).unwrap();
        let s2 = Store::open(tmp("home-")).unwrap();
        let b2 = resolve_base(&input, Some(&mut lock), &ResolveBaseOptions { frozen: true, ..opts(&s2) }).unwrap();
        assert_eq!(tag_calls(), before, "the tag was not resolved again");
        assert_eq!(b2.manifest_digest, b1.manifest_digest);

        // --update-lock re-resolves
        let s3 = Store::open(tmp("home-")).unwrap();
        let b3 = resolve_base(&input, Some(&mut lock), &ResolveBaseOptions { update: true, ..opts(&s3) }).unwrap();
        assert_eq!(b3.manifest_digest, v2.manifest_digest);
        assert_eq!(lock.get_image(&key).unwrap().digest, v2.manifest_digest);

        // a populated cache needs no network at all
        let n = reg.requests().len();
        resolve_base(&input, Some(&mut lock), &ResolveBaseOptions { frozen: true, ..opts(&s3) }).unwrap();
        assert_eq!(reg.requests().len(), n, "second resolution made no registry requests");
    }

    #[test]
    fn no_lockfile_warns_that_the_tag_is_not_pinned() {
        let reg = MockRegistry::start(MockOptions::default());
        seed_base(&reg, "1");
        let store = Store::open(tmp("home-")).unwrap();
        let warnings = RefCell::new(Vec::<String>::new());
        let warn = |m: &str| warnings.borrow_mut().push(m.to_string());
        resolve_base(&format!("{}/lib/base:1", reg.host), None, &ResolveBaseOptions { warn: Some(&warn), ..opts(&store) }).unwrap();
        assert!(warnings.borrow().iter().any(|w| w.contains("not pinned and there is no lockfile")), "{:?}", warnings.borrow());
    }

    #[test]
    fn platform_mismatches_are_errors() {
        let reg = MockRegistry::start(MockOptions::default());
        reg.seed_image("lib/arm", "1", SeedImage { layers: vec![vec![("/a", "a")]], arch: "arm64", ..Default::default() });
        let input = format!("{}/lib/arm:1", reg.host);
        let store = Store::open(tmp("home-")).unwrap();
        let e = resolve_base(&input, None, &opts(&store)).unwrap_err();
        assert!(e.message().contains("is linux/arm64, not linux/amd64; pass --platform"), "{}", e.message());
        let store = Store::open(tmp("home-")).unwrap();
        resolve_base(&input, None, &ResolveBaseOptions { platform: Some("linux/arm64".into()), ..opts(&store) }).unwrap();
    }

    #[test]
    fn bad_references_name_base() {
        let store = Store::open(tmp("home-")).unwrap();
        let span = SourceSpan::new("Sitefile", 3, 1);
        let e = resolve_base("Bad Ref", None, &ResolveBaseOptions { span: Some(span), ..opts(&store) }).unwrap_err();
        assert_eq!(e.code, E_USAGE);
        assert!(e.message().starts_with("Sitefile:3:1: BASE: invalid image reference"), "{}", e.message());
    }

    #[test]
    fn pushing_a_base_image_mounts_the_base_layers() {
        let reg = MockRegistry::start(MockOptions::default());
        let b = seed_base(&reg, "1");
        let store = Store::open(tmp("home-")).unwrap();
        let base = resolve_base(&format!("{}/lib/base:1", reg.host), None, &opts(&store)).unwrap();
        let o = OciOptions { root: Some("/usr/share/nginx/html".into()), base: Some(base), ..Default::default() };
        let id = store.add_image(&site(), &o).unwrap().id;
        let remote = format!("{}/me/web:1", reg.host);
        store.tag(&id, &parse_local_ref(&remote).unwrap()).unwrap();
        let p = push(&store, &remote, &PushOptions { env: Some(clean_env()), ..Default::default() }).unwrap();
        assert!(p.lines.iter().any(|l| l.contains("mounted from lib/base")), "{:?}", p.lines);
        assert_eq!((p.mounted, p.uploaded), (2, 2));
        let mounts: Vec<_> = reg.requests().into_iter().filter(|r| r.method == "POST" && r.url.contains("mount=")).collect();
        assert_eq!(mounts.len(), 2, "both base layers were mounted");
        assert!(mounts.iter().all(|m| m.url.contains("from=lib%2Fbase")));
        // and the result pulls back identically
        let s2 = Store::open(tmp("home-")).unwrap();
        crate::registry::pull(&s2, &remote, &crate::registry::PullOptions { env: Some(clean_env()), ..Default::default() }).unwrap();
        assert_eq!(s2.resolve(&remote).unwrap(), id);
        assert_eq!(s2.details(&id).unwrap().manifest.layers[..2].iter().map(|l| l.digest.clone()).collect::<Vec<_>>(), b.layer_digests);
    }
}

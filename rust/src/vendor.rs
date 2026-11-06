//! `sitefile vendor` (port of `src/vendor.ts`, issue #96): fetch every remote IMPORT and the BASE image
//! exactly as a build would (same network policy, INTEGRITY and lockfile checks, and Sitefile.lock is
//! updated the same way), then replace the vendor directory (default `vendor/sitefile`) with their
//! bytes under content-addressed names and a manifest. Always fetches: an existing vendor directory is
//! not read.

use crate::base_image::{resolve_base, ResolveBaseOptions};
use crate::digest::sha256;
use crate::error::{ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::resources::git::GitSelection;
use crate::resources::vendor::VendoredGit;
use crate::executor::{fetch_git_import, fetch_remote_import, git_selection_bytes, load_program, warn_unused_secrets, BuildOptions, Callback, LockfileSetting, Loaded, RemoteEnv};
use crate::secrets::{redact_error, redact_secrets};
use crate::export::oci::parse_manifest;
use crate::language::ast::ExportFormat;
use crate::planner::{plan, ImportLocation, StageBase};
use crate::refs::{parse_ref, with_default_registry};
use crate::registry::RegistryOptions;
use crate::resources::lockfile::Lockfile;
use crate::resources::vendor::{serialize_vendor_manifest, vendor_dir_rel, write_vendor_dir, VendorManifest, VendoredImage, VendoredResource, DEFAULT_VENDOR_DIR};
use crate::store::Store;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub struct VendorResult {
    /// Path of the vendor directory written.
    pub dir: PathBuf,
    pub resources: usize,
    pub images: usize,
    pub blobs: usize,
    /// Total size of the blobs.
    pub bytes: u64,
    pub lockfile_written: bool,
    pub warnings: Vec<String>,
}

pub fn vendor(options: &BuildOptions) -> Result<VendorResult> {
    if options.secrets.is_empty() {
        return vendor_unredacted(options);
    }
    let secrets = options.secrets.clone();
    let mut redacted = options.clone();
    if let Some(on) = options.on_warning.clone() {
        let s = secrets.clone();
        redacted.on_warning = Some(Arc::new(move |m: &str| on(&redact_secrets(m, &s))));
    }
    match vendor_unredacted(&redacted) {
        Ok(r) => {
            let warnings = r.warnings.iter().map(|w| redact_secrets(w, &secrets)).collect();
            Ok(VendorResult { warnings, ..r })
        }
        Err(e) => Err(redact_error(e, &secrets)),
    }
}

fn vendor_unredacted(options: &BuildOptions) -> Result<VendorResult> {
    let Loaded { program, sitefile_path, label } = load_program(&options.context_dir, options.sitefile.as_deref(), options.label.as_deref())?;
    let p = plan(&program, &options.build_args)?;
    let warnings: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let warn: Callback = {
        let warnings = warnings.clone();
        let on = options.on_warning.clone();
        Arc::new(move |m: &str| {
            if let Ok(mut w) = warnings.lock() {
                w.push(m.to_string());
            }
            if let Some(f) = &on {
                f(m);
            }
        })
    };
    for w in &p.warnings {
        warn(w);
    }
    let dir = vendor_dir_rel(options.vendor_dir.as_deref().unwrap_or(DEFAULT_VENDOR_DIR))?;
    let context_real = std::fs::canonicalize(&options.context_dir)
        .map_err(|_| SitefileError::plain(E_NOT_FOUND, format!("build context '{}' does not exist", options.context_dir)))?;
    let lock_path = match &options.lockfile {
        LockfileSetting::Disabled => None,
        LockfileSetting::Path(p) => Some(p.clone()),
        LockfileSetting::Default => Some(sitefile_path.parent().unwrap_or(Path::new("/")).join("Sitefile.lock")),
    };
    let lock = Mutex::new(match lock_path {
        Some(p) => Some(Lockfile::load(p)?),
        None => None,
    });
    let frozen = options.frozen_lockfile;
    let update = options.update_lock;

    warn_unused_secrets(&p, &options.secrets, &warn);
    let mut blobs: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut m = VendorManifest::default();
    {
        let env = RemoteEnv {
            policy: &options.fetch_policy,
            lock: &lock,
            label: &label,
            warn: &warn,
            frozen,
            update,
            vendor: None,
            archive_limits: options.archive_limits,
            secrets: &options.secrets,
        };
        for spec in &p.imports {
            if let ImportLocation::Git(loc) = &spec.location {
                let (commit, selection) = fetch_git_import(spec, loc, &env)?;
                let bytes = git_selection_bytes(&selection)?;
                let digest = sha256(&bytes);
                let mode = match &selection {
                    GitSelection::File { mode, .. } => Some(if *mode == 0o755 { "755" } else { "644" }.to_string()),
                    GitSelection::Tree(_) => None,
                };
                m.git.insert(loc.url.clone(), VendoredGit { commit, kind: selection.kind().into(), integrity: digest.clone(), size: bytes.len() as u64, mode });
                blobs.insert(digest, bytes);
                continue;
            }
            let ImportLocation::Remote { url } = &spec.location else { continue };
            let (bytes, final_url) = fetch_remote_import(spec, url, &env)?;
            let digest = sha256(&bytes);
            m.resources.insert(url.clone(), VendoredResource { integrity: digest.clone(), size: bytes.len() as u64, resolved: final_url });
            blobs.insert(digest, bytes);
        }
    }

    let mut lock = lock.into_inner().unwrap_or(None);
    // FROM image stage bases with a registry (local-store images are not vendored), then the BASE.
    let mut wanted: Vec<(String, String, bool, Option<SourceSpan>)> = Vec::new();
    for st in &p.stages {
        if let StageBase::Image { reference, local: false, lead, span, .. } = &st.base {
            wanted.push((reference.clone(), format!("{lead} image"), false, Some(span.clone())));
        }
    }
    if let Some(input) = options.base_image.clone().or_else(|| if p.export_format == ExportFormat::Oci { p.export_base.clone() } else { None }) {
        wanted.push((input, "BASE".into(), true, None));
    }
    let mut store: Option<Store> = None;
    for (input, lead, enforce, span) in wanted {
        if store.is_none() {
            store = Some(Store::open_default()?);
        }
        let store = store.as_ref().expect("opened above");
        let platform = options.platform.clone().unwrap_or_else(|| "linux/amd64".into());
        let w = warn.clone();
        let warn_fn = move |msg: &str| w(msg);
        let o = ResolveBaseOptions {
            platform: Some(platform.clone()),
            frozen,
            update,
            registry: RegistryOptions {
                insecure_registries: options.insecure_registries.clone(),
                allow_private_network: options.fetch_policy.allow_private_network,
                ..Default::default()
            },
            warn: Some(&warn_fn),
            span,
            lead: Some(lead),
            enforce_platform: Some(enforce),
            ..ResolveBaseOptions::new(store)
        };
        let base = resolve_base(&input, lock.as_mut(), &o)?;
        // resolve_base has validated the reference; this is its lockfile key.
        let r = with_default_registry(&parse_ref(&input)?);
        let registry = r.registry.clone().unwrap_or_default();
        let lock_key = format!("{registry}/{}:{}", r.name, r.tag.clone().unwrap_or_else(|| "latest".into()));
        let root = r.digest.clone().or_else(|| lock.as_ref().and_then(|l| l.get_image(&lock_key)).map(|e| e.digest.clone())).unwrap_or_else(|| base.manifest_digest.clone());
        let mut add = |digest: &str, size: Option<u64>| -> Result<Vec<u8>> {
            let bytes = store.read_blob(digest, size)?;
            blobs.insert(digest.to_string(), bytes.clone());
            Ok(bytes)
        };
        add(&root, None)?;
        let manifest_bytes = add(&base.manifest_digest, None)?;
        let parsed = parse_manifest(&manifest_bytes).map_err(|e| SitefileError::plain(E_STORE, format!("{}: {e}", base.manifest_digest)))?;
        for d in std::iter::once(&parsed.config).chain(parsed.layers.iter()) {
            add(&d.digest, Some(d.size))?;
        }
        let key = match &r.digest {
            Some(d) => format!("{registry}/{}{}@{d}", r.name, r.tag.as_ref().map(|t| format!(":{t}")).unwrap_or_default()),
            None => lock_key,
        };
        m.images.insert(key, VendoredImage { digest: root, platform, manifest: base.manifest_digest.clone() });
    }

    let text = serialize_vendor_manifest(&m);
    let written = write_vendor_dir(&context_real, &dir, &text, &blobs)?;
    let lockfile_written = match lock.as_mut() {
        Some(l) if !frozen => l.save()?,
        _ => false,
    };
    let warnings = warnings.lock().map(|w| w.clone()).unwrap_or_default();
    Ok(VendorResult {
        dir: written,
        resources: m.resources.len() + m.git.len(),
        images: m.images.len(),
        blobs: blobs.len(),
        bytes: blobs.values().map(|b| b.len() as u64).sum(),
        lockfile_written,
        warnings,
    })
}

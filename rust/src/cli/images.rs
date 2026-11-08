//! `images`, `image inspect|prune|diff`, `tag`, `rmi`, `save` (port of `src/cli/images.ts`).

use super::args::{flag, flag_s, string, string_s, OptSpec, Parsed};
use super::common::{
    err, human_size, now_ms, out, pretty_json, relative_time, require_args, table, usage, Command,
};
use crate::error::{ErrorCode, Result};
use crate::executor::relative_path;
use crate::export::archive_writers::{source_date_epoch, write_tar, WriterOptions};
use crate::export::exporter::{export_directory, export_file, random_hex, ExportOptions};
use crate::export::formats::{
    compression_from_extension, encode_tree, format_from_extension, is_archive_format,
    EncodeOptions,
};
use crate::export::oci::{media, site_layer_index, Descriptor};
use crate::language::ast::ExportFormat;
use crate::refs::{looks_like_image_id, parse_local_ref, ref_key};
use crate::store::{diff_trees, short_id, Store};
use serde_json::{json, Map, Value};
use std::path::Path;

pub const EXPORT_FORMATS: &str = "directory, zip, tar, tgz, wbn, oci";

fn split(key: &str) -> (String, String) {
    let i = key.rfind(':').unwrap_or(key.len());
    (
        key[..i].to_string(),
        key.get(i + 1..).unwrap_or("").to_string(),
    )
}

/// A path relative to the cwd when it is inside it, else absolute.
fn shown(p: &Path) -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    let rel = relative_path(&cwd, p);
    if rel.is_empty() {
        ".".into()
    } else if rel.starts_with("..") {
        p.display().to_string()
    } else {
        rel
    }
}

pub fn descriptor_json(d: &Descriptor) -> Value {
    let mut m = Map::new();
    m.insert("mediaType".into(), Value::String(d.media_type.clone()));
    m.insert("digest".into(), Value::String(d.digest.clone()));
    m.insert("size".into(), Value::from(d.size));
    if let Some(a) = &d.annotations {
        m.insert(
            "annotations".into(),
            Value::Object(
                a.iter()
                    .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                    .collect(),
            ),
        );
    }
    Value::Object(m)
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

const IMAGES_OPTS: &[OptSpec] = &[flag("json"), flag("digests"), flag_s("quiet", 'q')];
pub const IMAGES: Command = Command {
    name: "images",
    summary: "list images in the local store",
    usage: "images [--json] [--digests] [-q]",
    description: "List the images in the local store (SITEFILE_HOME or ~/.sitefile), newest first.\nImages without a tag show as <none>. Alias: sitefile image ls.",
    flags: &["--json         machine-readable output (full IDs, sizes in bytes)", "--digests      add the full image ID (sha256:...) column", "-q, --quiet    only print short image IDs"],
    options: IMAGES_OPTS,
    run: run_images,
};

fn run_images(v: &Parsed) -> Result<i32> {
    require_args(&v.positionals, 0, 0, "no arguments")?;
    let store = Store::open_default()?;
    let list = store.list()?;
    let mut rows = Vec::new();
    for im in &list {
        if im.tags.is_empty() {
            rows.push((im, None));
        } else {
            for t in &im.tags {
                rows.push((im, Some(t.clone())));
            }
        }
    }
    if v.flag("json") {
        let arr: Vec<Value> = rows
            .iter()
            .map(|(im, tag)| {
                let (repo, t) = match tag {
                    Some(t) => {
                        let (r, t) = split(t);
                        (Value::String(r), Value::String(t))
                    }
                    None => (Value::Null, Value::Null),
                };
                json!({ "repository": repo, "tag": t, "id": im.id, "created": im.created, "files": im.files, "size": im.size })
            })
            .collect();
        out(&(pretty_json(&Value::Array(arr)) + "\n"));
        return Ok(0);
    }
    if v.flag("quiet") {
        let mut seen = std::collections::HashSet::new();
        for (im, _) in &rows {
            if seen.insert(im.id.clone()) {
                out(&format!("{}\n", short_id(&im.id)));
            }
        }
        return Ok(0);
    }
    let digests = v.flag("digests");
    let mut head: Vec<String> = vec!["REPOSITORY".into(), "TAG".into(), "IMAGE ID".into()];
    if digests {
        head.push("DIGEST".into());
    }
    head.extend(["CREATED".into(), "FILES".into(), "SIZE".into()]);
    let now = now_ms();
    let mut body = vec![head];
    for (im, tag) in &rows {
        let (repo, t) = match tag {
            Some(t) => split(t),
            None => ("<none>".into(), "<none>".into()),
        };
        let mut r = vec![repo, t, short_id(&im.id)];
        if digests {
            r.push(im.id.clone());
        }
        r.extend([
            relative_time(&im.created, now),
            im.files.to_string(),
            human_size(im.size),
        ]);
        body.push(r);
    }
    out(&table(&body));
    Ok(0)
}

const INSPECT_OPTS: &[OptSpec] = &[flag("json")];
pub const INSPECT: Command = Command {
    name: "image inspect",
    summary: "show an image manifest",
    usage: "image inspect <ref|id> [--json]",
    description:
        "Show an image: its ID, tags, creation time, build metadata (ARG names only) and files.",
    flags: &["--json   machine-readable output"],
    options: INSPECT_OPTS,
    run: run_inspect,
};

fn run_inspect(v: &Parsed) -> Result<i32> {
    require_args(&v.positionals, 1, 1, "an image reference")?;
    let store = Store::open_default()?;
    let id = store.resolve(&v.positionals[0])?;
    let d = store.details(&id)?;
    let tree = store.load_tree(&id)?; // verifies every digest
    let files: Vec<(String, String, usize, String)> = tree
        .entries()
        .map(|(p, n)| {
            (
                p.clone(),
                format!("{:o}", n.mode),
                n.bytes.len(),
                n.digest.clone(),
            )
        })
        .collect();
    let layer = d.manifest.layers.first();
    if v.flag("json") {
        let labels: Map<String, Value> = d
            .config
            .labels
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        let o = json!({
            "id": id,
            "tags": d.tags,
            "created": d.created,
            "os": d.config.os,
            "architecture": d.config.architecture,
            "root": d.root,
            "size": d.size,
            "labels": labels,
            "config": descriptor_json(&d.manifest.config),
            "layers": d.manifest.layers.iter().map(descriptor_json).collect::<Vec<_>>(),
            "diffIds": d.config.diff_ids,
            "files": files.iter().map(|(p, m, s, dg)| json!({ "path": p, "mode": m, "size": s, "digest": dg })).collect::<Vec<_>>(),
        });
        out(&(pretty_json(&o) + "\n"));
        return Ok(0);
    }
    let mut lines = vec![
        format!("ID:          {id}"),
        format!(
            "Tags:        {}",
            if d.tags.is_empty() {
                "<none>".to_string()
            } else {
                d.tags.join(", ")
            }
        ),
        format!(
            "Stored:      {} ({})",
            d.created,
            relative_time(&d.created, now_ms())
        ),
        format!("Platform:    {}/{}", d.config.os, d.config.architecture),
        format!("Root:        {}", d.root),
        format!(
            "Layer:       {} ({} {})",
            layer.map(|l| l.digest.as_str()).unwrap_or("-"),
            human_size(d.layer_size),
            match site_layer_index(&d.manifest, &d.config).and_then(|i| d.manifest.layers.get(i)) {
                Some(l) if l.media_type == media::LAYER_TAR => "tar",
                _ => "gzip",
            }
        ),
        format!(
            "Diff ID:     {}",
            d.config.diff_ids.first().map(String::as_str).unwrap_or("-")
        ),
        format!("Config:      {}", d.manifest.config.digest),
        "Labels:".to_string(),
    ];
    for (k, val) in &d.config.labels {
        lines.push(format!("  {k}={val}"));
    }
    lines.push(format!(
        "Files:       {} ({})",
        files.len(),
        human_size(d.size)
    ));
    lines.push(String::new());
    for (p, m, s, _) in &files {
        lines.push(format!("{m}  {s:>8}  {p}"));
    }
    out(&(lines.join("\n") + "\n"));
    Ok(0)
}

pub const TAG: Command = Command {
    name: "tag",
    summary: "add a name:tag to an image",
    usage: "tag <src> <dst>",
    description: "Create the local reference <dst> (name[:tag]) for the image <src> (reference or ID).\nAn existing <dst> is moved to the new image.",
    flags: &[],
    options: &[],
    run: run_tag,
};

fn run_tag(v: &Parsed) -> Result<i32> {
    require_args(&v.positionals, 2, 2, "<src> and <dst>")?;
    let dst = parse_local_ref(&v.positionals[1])?;
    let store = Store::open_default()?;
    let id = store.resolve(&v.positionals[0])?;
    let key = store.tag(&id, &dst)?;
    out(&format!("{key} -> {}\n", short_id(&id)));
    Ok(0)
}

const RMI_OPTS: &[OptSpec] = &[flag_s("force", 'f')];
pub const RMI: Command = Command {
    name: "rmi",
    summary: "remove tags / images",
    usage: "rmi [-f] <ref|id>...",
    description: "Remove the tag <ref>. The image manifest is deleted when no other tag references it (blobs are\nreclaimed by \"sitefile image prune\"). Removing by image ID untags every tag and needs -f when there is more than one.",
    flags: &["-f, --force   remove an image by ID even if it has several tags"],
    options: RMI_OPTS,
    run: run_rmi,
};

fn run_rmi(v: &Parsed) -> Result<i32> {
    require_args(&v.positionals, 1, usize::MAX, "an image reference")?;
    let store = Store::open_default()?;
    let mut failed = false;
    for r in &v.positionals {
        match store.remove_image(r, v.flag("force")) {
            Ok(res) => {
                for t in &res.untagged {
                    out(&format!("Untagged: {t}\n"));
                }
                if let Some(d) = &res.deleted {
                    out(&format!("Deleted: {d}\n"));
                }
            }
            Err(e) => {
                if e.code == ErrorCode::E_USAGE {
                    return Err(e);
                }
                err(&format!("error[{}]: {}\n", e.code, e.message()));
                failed = true;
            }
        }
    }
    Ok(if failed { 1 } else { 0 })
}

pub const PRUNE: Command = Command {
    name: "image prune",
    summary: "delete unreferenced manifests and blobs",
    usage: "image prune",
    description: "Delete untagged image manifests and every blob no remaining image uses, then print the space reclaimed.",
    flags: &[],
    options: &[],
    run: run_prune,
};

fn run_prune(v: &Parsed) -> Result<i32> {
    require_args(&v.positionals, 0, 0, "no arguments")?;
    let store = Store::open_default()?;
    let r = store.prune()?;
    out(&format!(
        "Deleted {} and {}\nTotal reclaimed space: {}\n",
        plural(r.manifests, "image", "images"),
        plural(r.blobs, "blob", "blobs"),
        human_size(r.bytes)
    ));
    Ok(0)
}

const SAVE_OPTS: &[OptSpec] = &[
    string_s("out", 'o'),
    string("format"),
    string("base-url"),
    flag("force"),
];
pub const SAVE: Command = Command {
    name: "save",
    summary: "export an image to a directory or archive",
    usage: "save <ref|id> -o <out> [--format fmt] [--base-url url] [--force]",
    description: "Write an image's files to a directory, or to a zip, tar, tar.gz (tgz) or wbn archive or an IPFS CAR, or the image itself as\nan OCI archive (-o x.oci.tar) or OCI layout directory (--format oci). The format is chosen like \"build -o\":\nfrom --format, else the extension (.zip .tar .tar.gz .tgz .wbn .wbn.gz .oci.tar .car), else a directory; a wbn\nwritten to *.wbn.gz is gzipped.\nArchives are deterministic (set SOURCE_DATE_EPOCH to stamp them). A non-empty destination needs --force.",
    flags: &[
        "-o, --out <path>     destination (required)",
        "    --format <fmt>   directory | zip | tar | tgz | wbn | oci (alias tar.gz)",
        "    --base-url <url> https URL ending in \"/\" (wbn only; required for wbn)",
        "    --force          replace a non-empty destination or an existing file",
    ],
    options: SAVE_OPTS,
    run: run_save,
};

/// Removes a scratch directory when dropped.
struct Scratch(std::path::PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run_save(v: &Parsed) -> Result<i32> {
    require_args(&v.positionals, 1, 1, "an image reference")?;
    let Some(out_path) = v.str("out").filter(|s| !s.is_empty()) else {
        return usage("save requires -o <out>");
    };
    let mut format: Option<ExportFormat> = None;
    if v.has("format") {
        format = ExportFormat::parse(v.str("format").unwrap_or(""));
        if format.is_none() {
            return usage(format!(
                "--format expects one of {EXPORT_FORMATS} (or tar.gz)"
            ));
        }
    }
    let format = format
        .or_else(|| format_from_extension(out_path))
        .unwrap_or(ExportFormat::Directory);
    let store = Store::open_default()?;
    let input = &v.positionals[0];
    let id = store.resolve(input)?;
    // An empty scratch directory as the "context": the destination is then always
    // outside it, so a non-empty directory or an existing file needs --force.
    let dir = std::env::temp_dir().join(format!("sitefile-save-{}", random_hex(3)));
    std::fs::create_dir_all(&dir).map_err(|e| {
        crate::error::SitefileError::plain(
            ErrorCode::E_EXPORT,
            format!("cannot create a temporary directory: {e}"),
        )
    })?;
    let scratch = Scratch(crate::export::exporter::resolve_path(&dir));
    let opts = ExportOptions::new(&scratch.0).force(v.flag("force"));
    let out_p = Path::new(out_path);
    if format == ExportFormat::Oci {
        // The stored image as-is (same digests): an OCI archive for *.tar, else a layout directory.
        let ref_name = if looks_like_image_id(input) {
            None
        } else {
            Some(ref_key(&parse_local_ref(input)?))
        };
        let layout = store.layout_of(&id, ref_name.as_deref())?;
        if out_path.to_lowercase().ends_with(".tar") {
            let data = write_tar(
                &layout,
                &WriterOptions {
                    epoch: Some(source_date_epoch()?.unwrap_or(0)),
                    dirs: Vec::new(),
                },
            )?;
            let w = export_file(&data, out_p, &opts)?;
            out(&format!(
                "Saved {} to {} (oci archive, {} bytes)\n",
                short_id(&id),
                shown(&w.out_file),
                w.bytes
            ));
        } else {
            let w = export_directory(&layout, out_p, &opts)?;
            out(&format!(
                "Saved {} to {} (oci layout)\n",
                short_id(&id),
                shown(&w.out_dir)
            ));
        }
        return Ok(0);
    }
    let tree = store.load_tree(&id)?;
    if !is_archive_format(format) {
        let w = export_directory(&tree, out_p, &opts)?;
        out(&format!(
            "Saved {} ({} files) to {}\n",
            short_id(&id),
            tree.len(),
            shown(&w.out_dir)
        ));
    } else {
        let car = if format == ExportFormat::Car {
            Some(crate::export::car::encode_car(&tree)?)
        } else {
            None
        };
        let data = if let Some((bytes, _)) = &car {
            bytes.clone()
        } else {
            encode_tree(
                &tree,
                format,
                &EncodeOptions {
                    writer: WriterOptions {
                        epoch: source_date_epoch()?,
                        dirs: Vec::new(),
                    },
                    base_url: v.str("base-url").map(str::to_string),
                    compression: compression_from_extension(out_path, format),
                },
            )?
        };
        let w = export_file(&data, out_p, &opts)?;
        let name = if format == ExportFormat::Tgz {
            "tar.gz"
        } else {
            format.as_str()
        };
        out(&format!(
            "Saved {} ({} files) to {} ({name}, {} bytes{})\n",
            short_id(&id),
            tree.len(),
            shown(&w.out_file),
            w.bytes,
            car.map(|(_, r)| format!(", root {r}")).unwrap_or_default()
        ));
    }
    drop(scratch);
    Ok(0)
}

pub const DIFF: Command = Command {
    name: "image diff",
    summary: "compare two images",
    usage: "image diff <ref|id> <ref|id>",
    description: "List paths added (A), removed (D) or changed (C) going from the first image to the second.\nExit status is 0 whether or not they differ.",
    flags: &[],
    options: &[],
    run: run_diff,
};

fn run_diff(v: &Parsed) -> Result<i32> {
    require_args(&v.positionals, 2, 2, "two image references")?;
    let store = Store::open_default()?;
    let a = store.load_tree(&store.resolve(&v.positionals[0])?)?;
    let b = store.load_tree(&store.resolve(&v.positionals[1])?)?;
    let lines = diff_trees(&a, &b);
    out(&if lines.is_empty() {
        String::new()
    } else {
        lines.join("\n") + "\n"
    });
    Ok(0)
}

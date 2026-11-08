//! `sitefile verify` and `sitefile diff` (port of `src/cli/integrity.ts`).

use super::args::{flag, string, OptSpec, Parsed};
use super::common::{out, pretty_json, require_args, usage, Command};
use crate::error::{ErrorCode::*, Result, SitefileError};
use crate::integrity::{
    diff_trees, load_target, manifest_of, parse_manifest, path_inside, resolve_target, verify_tree,
    LazyStore, Target,
};

const VERIFY_OPTS: &[OptSpec] = &[string("manifest"), flag("json")];

pub const VERIFY: Command = Command {
    name: "verify",
    summary: "check a directory, archive or image against a file manifest",
    usage: "verify <dir|archive|ref|id> [--manifest <file>] [--json]",
    description: "Compare a deployed or extracted copy with the manifest written by \"build --manifest\" or \"save --manifest\":\nevery file the manifest lists must be there with the same size and SHA-256, and no other file may be.\nPrints one line per difference (missing, changed, extra) and fails with E_INTEGRITY when there is any.\nA directory is read like a build context (hidden files count; .git is skipped); an archive is read from\nits bytes (zip, tar, tar.gz, wbn, OCI archive). A manifest file inside the verified directory is not\ncounted as extra. Without --manifest the target must be an image: its stored blobs are checked.",
    flags: &[
        "--manifest <file>  the manifest to check against (JSON, see docs/CLI.md)",
        "--json             machine-readable report",
    ],
    options: VERIFY_OPTS,
    run: run_verify,
};

fn run_verify(v: &Parsed) -> Result<i32> {
    require_args(
        &v.positionals,
        1,
        1,
        "a directory, archive file or image reference",
    )?;
    let arg = &v.positionals[0];
    let mut store = LazyStore::default();
    let target = resolve_target(arg, &mut store)?;
    let manifest_file = v.str("manifest");
    if manifest_file == Some("") {
        return usage("--manifest expects a file path");
    }
    if manifest_file.is_none() && !matches!(target, Target::Image(_)) {
        return usage("verify needs --manifest <file> for a directory or an archive");
    }
    let tree = load_target(&target, &mut store)?;
    let mut ignore = None;
    let manifest = match manifest_file {
        Some(file) => {
            let bytes = std::fs::read(file).map_err(|e| {
                SitefileError::plain(
                    if e.kind() == std::io::ErrorKind::NotFound {
                        E_NOT_FOUND
                    } else {
                        E_INTEGRITY
                    },
                    format!("cannot read the manifest '{file}'"),
                )
            })?;
            let m = parse_manifest(&bytes, file)?;
            if let Target::Dir(d) = &target {
                ignore = path_inside(d, file);
            }
            m
        }
        // load_tree checked every blob against its digest
        None => manifest_of(&tree),
    };
    let r = verify_tree(&tree, &manifest, ignore.as_deref());
    out(&if v.flag("json") {
        pretty_json(&r.to_json()) + "\n"
    } else if r.ok() {
        format!(
            "OK: {} file{} match {}\n",
            r.files,
            if r.files == 1 { "" } else { "s" },
            r.root
        )
    } else {
        r.text()
    });
    if !r.ok() {
        return Err(SitefileError::plain(
            E_INTEGRITY,
            format!(
                "'{arg}' does not match the manifest: {} missing, {} changed, {} extra",
                r.missing.len(),
                r.changed.len(),
                r.extra.len()
            ),
        ));
    }
    Ok(0)
}

const DIFF_OPTS: &[OptSpec] = &[flag("json"), flag("exit-code")];

pub const DIFF: Command = Command {
    name: "diff",
    summary: "files changed between two images, directories or archives",
    usage: "diff <a> <b> [--json] [--exit-code]",
    description: "List the files added (A), removed (D) and changed (C: content or mode) going from <a> to <b>, with their\nsizes, then the rules added (+) and removed (-) in _redirects and _headers. Each side is a directory, an\narchive file (zip, tar, tar.gz, wbn, OCI archive) or an image reference or ID, read as \"sitefile verify\"\nreads it (hidden files count). Digests, not timestamps, decide what changed.",
    flags: &[
        "--json        machine-readable report (sizes in bytes, digests, modes)",
        "--exit-code   exit 1 when the two differ (default 0 either way)",
    ],
    options: DIFF_OPTS,
    run: run_diff,
};

fn run_diff(v: &Parsed) -> Result<i32> {
    require_args(
        &v.positionals,
        2,
        2,
        "two images, directories or archive files",
    )?;
    let mut store = LazyStore::default();
    let a = resolve_target(&v.positionals[0], &mut store)?;
    let b = resolve_target(&v.positionals[1], &mut store)?;
    let ta = load_target(&a, &mut store)?;
    let tb = load_target(&b, &mut store)?;
    let r = diff_trees(&ta, &tb);
    out(&if v.flag("json") {
        pretty_json(&r.to_json()) + "\n"
    } else {
        r.text()
    });
    Ok(if v.flag("exit-code") && !r.is_empty() {
        1
    } else {
        0
    })
}

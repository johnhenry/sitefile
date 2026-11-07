//! `sitefile build https://host/repo.git#ref:dir` (port of `src/cli/git-context.ts`, issue #91): a
//! build context fetched from Git. A context argument with a URL scheme (`https://`, `http://`,
//! `git+https://`, `git+http://`) is a Git URL. Its tree is fetched like a Git IMPORT (same network
//! policy; symlinks resolved inside the tree, submodules refused), written to a temporary directory,
//! built from there, and removed.

use crate::error::{ErrorCode::*, Result, SitefileError};
use crate::resources::git::{
    fetch_git_commit, parse_git_uri, select_git_path, GitLocation, GitSelection, GitTransport,
};
use crate::resources::types::{ArchiveLimits, FetchPolicy};
use crate::tree::{make_file, Tree};
use std::path::{Path, PathBuf};

/// `^[A-Za-z][A-Za-z0-9+.-]*://`
pub fn is_git_context(context: &str) -> bool {
    let Some((scheme, rest)) = context.split_once(':') else {
        return false;
    };
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
        && rest.starts_with("//")
}

/// The location of a Git context argument (`https://…` is read as `git+https://…`).
pub fn git_context_location(context: &str) -> Result<GitLocation> {
    let uri = if context
        .get(..4)
        .is_some_and(|p| p.eq_ignore_ascii_case("git+"))
    {
        context.to_string()
    } else {
        format!("git+{context}")
    };
    let mut loc = parse_git_uri(&uri, None)?;
    loc.url = context.to_string();
    Ok(loc)
}

/// A fetched context directory; removed when dropped.
pub struct GitContext {
    pub dir: PathBuf,
    pub commit: String,
}

impl Drop for GitContext {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn temp_dir() -> Result<PathBuf> {
    let base = std::env::temp_dir();
    for _ in 0..16 {
        let p = base.join(format!(
            "sitefile-context-{}",
            crate::export::exporter::random_hex(6)
        ));
        match std::fs::create_dir(&p) {
            Ok(()) => return Ok(p),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(SitefileError::plain(
                    E_EXPORT,
                    format!("cannot write the Git context to a temporary directory: {e}"),
                ))
            }
        }
    }
    Err(SitefileError::plain(
        E_EXPORT,
        "cannot write the Git context to a temporary directory: no unique name",
    ))
}

/// Fetch the context's tree and write it to a new temporary directory.
pub fn materialize_git_context(
    context: &str,
    policy: &FetchPolicy,
    limits: &ArchiveLimits,
) -> Result<GitContext> {
    let loc = git_context_location(context)?;
    let objs = fetch_git_commit(
        &loc.repo,
        &loc.git_ref,
        loc.commit.as_deref(),
        GitTransport::new(policy, None),
        limits,
    )?;
    let GitSelection::Tree(files) = select_git_path(&objs, &loc.repo, &loc.path, limits, None)?
    else {
        return Err(SitefileError::plain(
            E_TYPE,
            format!("build context '{context}' is a file; a Git context must name a directory"),
        ));
    };
    // Same checks as an IMPORT of the tree (NFC, case collisions), before anything is written.
    let tree = Tree::from_files(
        files
            .into_iter()
            .map(|(p, b, m)| (p, make_file(b, context, m))),
    )?;
    let ctx = GitContext {
        dir: temp_dir()?,
        commit: objs.commit,
    };
    let write = |dir: &Path| -> std::io::Result<()> {
        for (p, n) in tree.entries() {
            let mut dest = dir.to_path_buf();
            for seg in p[1..].split('/') {
                dest.push(seg);
            }
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&dest, &n.bytes)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(n.mode))?;
            }
        }
        Ok(())
    };
    if let Err(e) = write(&ctx.dir) {
        return Err(SitefileError::plain(
            E_EXPORT,
            format!("cannot write the Git context to a temporary directory: {e}"),
        ));
    }
    Ok(ctx)
}

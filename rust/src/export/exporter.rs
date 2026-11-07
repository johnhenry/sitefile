//! Writing artifacts to disk (port of `src/exporter.ts`): a tree as a
//! directory ([`export_directory`]) or bytes as a single file ([`export_file`]),
//! with the same destination safety rules and race hardening as TypeScript.

use crate::error::{ErrorCode::*, Result, SitefileError};
use crate::tree::Tree;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use super::incremental::{
    dirs_of, exported, stamp_of, try_incremental, Applied, SharedExportState, Stamp,
};

/// Test hook: called after the temp tree is written, right before the final
/// re-validation and swap. Not exposed by the CLI.
pub type BeforeSwap = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone, Default)]
pub struct ExportOptions {
    /// The build context. The destination may never be, or contain, it.
    pub context_dir: PathBuf,
    /// Replace a non-empty destination outside the context.
    pub force: bool,
    #[doc(hidden)]
    pub before_swap: Option<BeforeSwap>,
    /// Watch mode (directory exports only): what the previous export of this session left on
    /// disk. When the destination still holds exactly that, only the files that changed are
    /// written (see `export::incremental`); either way the export is recorded for the next one.
    pub incremental: Option<SharedExportState>,
    /// Test hook (incremental export): called after the changed files are staged, before any
    /// of them is moved into the destination.
    #[doc(hidden)]
    pub before_apply: Option<BeforeSwap>,
}

impl ExportOptions {
    pub fn new(context_dir: impl Into<PathBuf>) -> Self {
        ExportOptions {
            context_dir: context_dir.into(),
            force: false,
            before_swap: None,
            incremental: None,
            before_apply: None,
        }
    }
    pub fn incremental(mut self, state: Option<SharedExportState>) -> Self {
        self.incremental = state;
        self
    }
    pub fn force(mut self, force: bool) -> Self {
        self.force = force;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportResult {
    /// Real absolute path the tree was written to.
    pub out_dir: PathBuf,
    pub files: usize,
    /// Set when only the changes were applied in place (incremental export).
    pub incremental: Option<Applied>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportFileResult {
    /// Real absolute path of the file that was written.
    pub out_file: PathBuf,
    pub bytes: usize,
}

/// `^\..+\.sitefile-(?:tmp|old)-[0-9a-f]+$`: the exporter's temporary and backup sibling names.
pub fn matches_temp_pattern(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('.') else {
        return false;
    };
    for kind in [".sitefile-tmp-", ".sitefile-old-"] {
        // `.+` is greedy: any occurrence with a non-empty part before it and hex after it matches.
        let mut from = 0;
        while let Some(i) = rest[from..].find(kind) {
            let at = from + i;
            let hex = &rest[at + kind.len()..];
            if at > 0
                && !hex.is_empty()
                && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            {
                return true;
            }
            from = at + 1;
        }
    }
    false
}

/// True if `name` is a temp/backup sibling the exporter creates for `out_basename`.
pub fn is_exporter_temp_name(name: &str, out_basename: &str) -> bool {
    let tmp = format!(".{out_basename}.sitefile-tmp-");
    let old = format!(".{out_basename}.sitefile-old-");
    (name.starts_with(&tmp) || name.starts_with(&old)) && matches_temp_pattern(name)
}

/// Random lowercase hex of `n` bytes from the OS (falls back to time and a counter).
pub fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    let ok = fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if !ok {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seed = format!(
            "{t}:{}:{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let d = crate::digest::sha256(seed.as_bytes());
        return d[7..7 + 2 * n.min(32)].to_string();
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

pub(super) fn show(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// The temp directory starts empty and the tree has no two paths equal under
/// case folding, so AlreadyExists/NotADirectory/IsADirectory here means the
/// destination file system treats two different tree paths as one name (a
/// folding or normalization rule wider than Sitefile's). Report the artifact
/// path, not the temp path; other errors are returned unchanged.
pub fn occupied(e: io::Error, p: &str) -> std::result::Result<SitefileError, io::Error> {
    match e.kind() {
        io::ErrorKind::AlreadyExists | io::ErrorKind::NotADirectory | io::ErrorKind::IsADirectory => Ok(SitefileError::plain(
            E_EXPORT,
            format!("cannot write '{p}': the destination file system already has an entry by that name (it folds case or normalizes Unicode more widely than Sitefile's checks)"),
        )),
        _ => Err(e),
    }
}

pub(super) fn unsafe_err(msg: String) -> SitefileError {
    SitefileError::plain(E_EXPORT_UNSAFE, msg)
}

/// `path.resolve`: absolute against the cwd, `.`/`..` resolved lexically, no trailing slash.
pub fn resolve_path(p: &Path) -> PathBuf {
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(p)
    };
    let mut out = PathBuf::from("/");
    for c in joined.components() {
        match c {
            Component::RootDir | Component::Prefix(_) | Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(s) => out.push(s),
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FileId {
    dev: u64,
    ino: u64,
}

pub(super) fn id_of(m: &fs::Metadata) -> FileId {
    FileId {
        dev: m.dev(),
        ino: m.ino(),
    }
}

/// Size and nanosecond ctime of a file: what [`FileId`] can't tell apart when an inode
/// number is reused for a replacement.
pub(super) fn replacement_stamp_of(m: &fs::Metadata) -> (u64, i64, i64) {
    (m.size(), m.ctime(), m.ctime_nsec())
}

pub(super) fn lstat_opt(p: &Path) -> io::Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(p) {
        Ok(m) => Ok(Some(m)),
        Err(e)
            if e.kind() == io::ErrorKind::NotFound || e.kind() == io::ErrorKind::NotADirectory =>
        {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

pub(super) fn is_real_dir(m: &fs::Metadata) -> bool {
    !m.file_type().is_symlink() && m.is_dir()
}

pub(super) fn is_real_file(m: &fs::Metadata) -> bool {
    !m.file_type().is_symlink() && m.is_file()
}

/// True if `child` is strictly inside `dir` (both absolute and normalized).
fn is_inside(child: &Path, dir: &Path) -> bool {
    child != dir && child.starts_with(dir)
}

fn fs_root(p: &Path) -> bool {
    p.parent().is_none()
}

fn realpath_or(p: &Path, what: &str) -> Result<PathBuf> {
    fs::canonicalize(p).map_err(|e| {
        SitefileError::plain(
            E_EXPORT,
            format!("cannot resolve {what} '{}': {e}", show(p)),
        )
    })
}

fn home_dir() -> PathBuf {
    let h = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"));
    fs::canonicalize(&h).unwrap_or_else(|_| resolve_path(&h))
}

/// The real location of `abs` without following a final symlink: realpath the
/// nearest existing ancestor of its parent and rejoin the rest.
fn realpath_missing(abs: &Path) -> Result<PathBuf> {
    if fs_root(abs) {
        return Ok(abs.to_path_buf());
    }
    let mut rest: Vec<std::ffi::OsString> = vec![abs
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_default()];
    let mut dir = abs.parent().unwrap_or(Path::new("/")).to_path_buf();
    loop {
        match fs::canonicalize(&dir) {
            Ok(real) => {
                let mut out = real;
                for r in rest.iter().rev() {
                    out.push(r);
                }
                return Ok(out);
            }
            Err(e)
                if e.kind() == io::ErrorKind::NotFound
                    || e.kind() == io::ErrorKind::NotADirectory =>
            {
                if fs_root(&dir) {
                    let mut out = dir.clone();
                    for r in rest.iter().rev() {
                        out.push(r);
                    }
                    return Ok(out);
                }
                rest.push(
                    dir.file_name()
                        .map(|s| s.to_os_string())
                        .unwrap_or_default(),
                );
                dir = dir.parent().unwrap_or(Path::new("/")).to_path_buf();
            }
            Err(e) => {
                return Err(SitefileError::plain(
                    E_EXPORT,
                    format!("cannot resolve '{}': {e}", show(&dir)),
                ))
            }
        }
    }
}

fn assert_safe_destination(out: &Path, context_real: &Path, home: &Path) -> Result<()> {
    if fs_root(out) {
        return Err(unsafe_err(format!(
            "refusing to export to the filesystem root '{}'",
            show(out)
        )));
    }
    if out == context_real {
        return Err(unsafe_err(format!(
            "refusing to export to the build context itself ('{}')",
            show(out)
        )));
    }
    if is_inside(context_real, out) {
        return Err(unsafe_err(format!(
            "refusing to export to '{}': it contains the build context '{}'",
            show(out),
            show(context_real)
        )));
    }
    if out == home || is_inside(home, out) {
        return Err(unsafe_err(format!(
            "refusing to export to '{}': it is or contains the home directory",
            show(out)
        )));
    }
    Ok(())
}

/// The parent must be a real directory (no symlink in its path); return its identity.
fn verified_parent_id(parent: &Path) -> Result<FileId> {
    let changed = |e: io::Error| {
        unsafe_err(format!(
            "export parent '{}' changed during export: {e}",
            show(parent)
        ))
    };
    let real = fs::canonicalize(parent).map_err(changed)?;
    if real != parent {
        return Err(unsafe_err(format!(
            "export parent '{}' resolves to '{}'; aborting",
            show(parent),
            show(&real)
        )));
    }
    let st = fs::metadata(parent).map_err(changed)?;
    if !st.is_dir() {
        return Err(unsafe_err(format!(
            "export parent '{}' is not a directory",
            show(parent)
        )));
    }
    Ok(id_of(&st))
}

pub(super) fn assert_parent_unchanged(parent: &Path, expected: FileId) -> Result<()> {
    if verified_parent_id(parent)? != expected {
        return Err(unsafe_err(format!(
            "export parent '{}' was replaced during export; aborting",
            show(parent)
        )));
    }
    Ok(())
}

/// Threads used for writing and removing many files (filesystem calls dominate
/// large exports; the TypeScript version makes them one at a time).
pub(super) fn io_threads(n: usize) -> usize {
    let cores = std::thread::available_parallelism()
        .map(|c| c.get())
        .unwrap_or(4);
    cores.min(8).min(n.max(1))
}

/// `remove_dir_all(p)`, with p's top-level entries removed in parallel.
/// `remove_dir_all` never follows symlinks, so only `p`'s own entries go.
fn remove_tree_parallel(p: &Path) {
    let entries: Vec<PathBuf> = match fs::read_dir(p) {
        Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).collect(),
        Err(_) => return,
    };
    let threads = io_threads(entries.len());
    if threads > 1 {
        let chunk = entries.len().div_ceil(threads);
        std::thread::scope(|sc| {
            for part in entries.chunks(chunk) {
                sc.spawn(move || {
                    for e in part {
                        match fs::symlink_metadata(e) {
                            Ok(m) if m.is_dir() => {
                                let _ = fs::remove_dir_all(e);
                            }
                            Ok(_) => {
                                let _ = fs::remove_file(e);
                            }
                            Err(_) => {}
                        }
                    }
                });
            }
        });
    }
    let _ = fs::remove_dir_all(p);
}

/// Recursively remove `p` only if it is a real directory with identity
/// `expected`; with no `expected`, only an empty real directory. Best effort.
pub(super) fn remove_if_same(p: &Path, expected: Option<FileId>) {
    let Ok(Some(st)) = lstat_opt(p) else { return };
    if !is_real_dir(&st) {
        return;
    }
    match expected {
        None => {
            let _ = fs::remove_dir(p);
        }
        Some(id) if id == id_of(&st) => {
            remove_tree_parallel(p);
        }
        Some(_) => {}
    }
}

/// Remove `p` only if it is a regular file with the identity we created.
fn remove_file_if_same(p: &Path, expected: FileId) {
    if let Ok(Some(st)) = lstat_opt(p) {
        if is_real_file(&st) && id_of(&st) == expected {
            let _ = fs::remove_file(p);
        }
    }
}

/// Destination checks shared by both exporters; returns (out_real, context_real, home, lstat of out).
fn check_destination(
    out: &Path,
    options: &ExportOptions,
) -> Result<(PathBuf, PathBuf, Option<fs::Metadata>)> {
    let out_real = realpath_missing(&resolve_path(out))?;
    let context_real = realpath_or(&resolve_path(&options.context_dir), "build context")?;
    let home = home_dir();
    assert_safe_destination(&out_real, &context_real, &home)?;
    let st = lstat_opt(&out_real).map_err(|e| {
        SitefileError::plain(
            E_EXPORT,
            format!("cannot inspect '{}': {e}", show(&out_real)),
        )
    })?;
    if let Some(s) = &st {
        if s.file_type().is_symlink() {
            // Never write through a final symlink; but if it points at something dangerous, say so.
            if let Ok(target) = fs::canonicalize(&out_real) {
                assert_safe_destination(&target, &context_real, &home)?;
            }
            return Err(SitefileError::plain(
                E_EXPORT,
                format!(
                    "export destination '{}' is a symbolic link",
                    show(&out_real)
                ),
            ));
        }
    }
    Ok((out_real, context_real, st))
}

pub(super) fn split_parent(out_real: &Path) -> (PathBuf, String) {
    let parent = out_real.parent().unwrap_or(Path::new("/")).to_path_buf();
    let base = out_real
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    (parent, base)
}

fn mkdir_parent(parent: &Path) -> Result<()> {
    fs::create_dir_all(parent).map_err(|e| {
        SitefileError::plain(E_EXPORT, format!("cannot create '{}': {e}", show(parent)))
    })
}

/// Materialize `tree` as a directory at `out_dir`. Files are written into a
/// fresh temporary sibling, which is then swapped in; on failure the previous
/// destination (if any) is left in place.
pub fn export_directory(
    tree: &Tree,
    out_dir: &Path,
    options: &ExportOptions,
) -> Result<ExportResult> {
    let entries: Vec<(&str, &[u8], u32, &str)> = tree
        .entries()
        .map(|(p, n)| (p.as_str(), &n.bytes[..], n.mode, n.digest.as_str()))
        .collect();
    export_entries(&entries, out_dir, options)
}

/// A validated file of the tree being exported.
pub(super) struct Planned<'a> {
    pub path: &'a str,
    pub segments: Vec<&'a str>,
    pub bytes: &'a [u8],
    pub mode: u32,
    pub digest: &'a str,
}

fn export_entries(
    entries: &[(&str, &[u8], u32, &str)],
    out_dir: &Path,
    options: &ExportOptions,
) -> Result<ExportResult> {
    let (out_real, context_real, st) = check_destination(out_dir, options)?;
    if let Some(s) = &st {
        if !s.is_dir() {
            return Err(SitefileError::plain(
                E_EXPORT,
                format!(
                    "export destination '{}' exists and is not a directory",
                    show(&out_real)
                ),
            ));
        }
        if !is_inside(&out_real, &context_real) && !options.force {
            let non_empty = fs::read_dir(&out_real)
                .map_err(|e| {
                    SitefileError::plain(
                        E_EXPORT,
                        format!("cannot read '{}': {e}", show(&out_real)),
                    )
                })?
                .next()
                .is_some();
            if non_empty {
                return Err(unsafe_err(format!(
                    "export destination '{}' is outside the build context and not empty; pass --force to replace it",
                    show(&out_real)
                )));
            }
        }
    }

    // --- validate paths before touching the disk
    let mut planned: Vec<Planned> = Vec::with_capacity(entries.len());
    for &(p, bytes, mode, digest) in entries {
        let Some(rel) = p.strip_prefix('/') else {
            return Err(SitefileError::plain(
                E_EXPORT,
                format!("invalid artifact path '{p}': not absolute"),
            ));
        };
        let segments: Vec<&str> = rel.split('/').collect();
        for seg in &segments {
            if seg.is_empty()
                || *seg == "."
                || *seg == ".."
                || seg.contains('\0')
                || seg.contains('\\')
            {
                return Err(SitefileError::plain(
                    E_EXPORT,
                    format!("invalid artifact path '{p}'"),
                ));
            }
        }
        planned.push(Planned {
            path: p,
            segments,
            bytes,
            mode,
            digest,
        });
    }

    // --- watch mode: apply only the changes when the destination is as we left it
    let state = options.incremental.clone();
    let mut state = state
        .as_ref()
        .map(|s| s.lock().unwrap_or_else(|e| e.into_inner()));
    if let Some(state) = state.as_deref_mut() {
        if let Some(applied) = try_incremental(&planned, &out_real, st.as_ref(), state, options) {
            return Ok(applied);
        }
    }
    let record = state.is_some();

    // --- write into a temp sibling, then swap (see src/exporter.ts for the
    // race-hardening rationale and residual risks; the steps are identical).
    let (parent, base) = split_parent(&out_real);
    let tmp = parent.join(format!(".{base}.sitefile-tmp-{}", random_hex(6)));
    let old = parent.join(format!(".{base}.sitefile-old-{}", random_hex(6)));
    mkdir_parent(&parent)?;
    let parent_id = verified_parent_id(&parent)?;
    let out_id = st.as_ref().map(id_of);

    fs::create_dir(&tmp).map_err(|e| {
        SitefileError::plain(E_EXPORT, format!("cannot create '{}': {e}", show(&tmp)))
    })?;
    let tmp_id = (|| -> Result<FileId> {
        let t = fs::symlink_metadata(&tmp).map_err(|e| {
            unsafe_err(format!(
                "temporary directory '{}' is not a real directory: {e}",
                show(&tmp)
            ))
        })?;
        if !is_real_dir(&t) {
            return Err(unsafe_err(format!(
                "temporary directory '{}' is not a real directory",
                show(&tmp)
            )));
        }
        assert_parent_unchanged(&parent, parent_id)?;
        Ok(id_of(&t))
    })();
    let tmp_id = match tmp_id {
        Ok(id) => id,
        Err(e) => {
            remove_if_same(&tmp, None);
            return Err(e);
        }
    };

    let mut old_moved = false;
    let mut swapped = false;
    let io_fail = |e: io::Error| {
        SitefileError::plain(
            E_EXPORT,
            format!("failed to export to '{}': {e}", show(&out_real)),
        )
    };
    let stamps: std::sync::Mutex<Vec<(usize, Stamp)>> = std::sync::Mutex::new(Vec::new());
    let result = (|| -> Result<()> {
        // Directories first, in order; then the files, in parallel.
        let targets: Vec<PathBuf> = planned
            .iter()
            .map(|e| {
                let mut t = tmp.clone();
                for s in &e.segments {
                    t.push(s);
                }
                t
            })
            .collect();
        let mut made: Option<&Path> = None;
        for (t, e) in targets.iter().zip(planned.iter()) {
            let dir = t.parent().unwrap_or(&tmp);
            if made != Some(dir) {
                fs::create_dir_all(dir)
                    .map_err(|err| occupied(err, e.path).unwrap_or_else(io_fail))?;
                made = Some(dir);
            }
        }
        // Watch mode records each file's stamp as written: renaming the temp directory leaves
        // its files' inodes (and ctimes) as they are now.
        let write_one = |target: &Path, bytes: &[u8], mode: u32| -> io::Result<Option<Stamp>> {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(target)?;
            f.write_all(bytes)?;
            f.set_permissions(fs::Permissions::from_mode(mode))?;
            Ok(if record {
                Some(stamp_of(&f.metadata()?))
            } else {
                None
            })
        };
        let jobs: Vec<(usize, &Path, &str, &[u8], u32)> = targets
            .iter()
            .zip(planned.iter())
            .enumerate()
            .map(|(i, (t, e))| (i, t.as_path(), e.path, e.bytes, e.mode))
            .collect();
        let threads = io_threads(jobs.len());
        let first_error: std::sync::Mutex<Option<(io::Error, &str)>> = std::sync::Mutex::new(None);
        std::thread::scope(|sc| {
            for part in jobs.chunks(jobs.len().div_ceil(threads).max(1)) {
                let (write_one, first_error, stamps) = (&write_one, &first_error, &stamps);
                sc.spawn(move || {
                    let mut mine = Vec::new();
                    for &(i, target, p, bytes, mode) in part {
                        if first_error.lock().map(|e| e.is_some()).unwrap_or(true) {
                            return;
                        }
                        match write_one(target, bytes, mode) {
                            Ok(Some(stamp)) => mine.push((i, stamp)),
                            Ok(None) => {}
                            Err(e) => {
                                let mut slot =
                                    first_error.lock().unwrap_or_else(|p| p.into_inner());
                                slot.get_or_insert((e, p));
                                return;
                            }
                        }
                    }
                    stamps
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .extend(mine);
                });
            }
        });
        if let Some((e, p)) = first_error.into_inner().unwrap_or_else(|p| p.into_inner()) {
            return Err(occupied(e, p).unwrap_or_else(io_fail));
        }
        if let Some(hook) = &options.before_swap {
            hook();
        }

        // Re-validate immediately before the swap.
        assert_parent_unchanged(&parent, parent_id)?;
        match lstat_opt(&tmp).map_err(io_fail)? {
            Some(t) if is_real_dir(&t) && id_of(&t) == tmp_id => {}
            _ => {
                return Err(unsafe_err(format!(
                    "temporary directory '{}' changed during export; aborting",
                    show(&tmp)
                )))
            }
        }
        let cur = lstat_opt(&out_real).map_err(io_fail)?;
        match (out_id, &cur) {
            (Some(id), Some(c)) if is_real_dir(c) && id_of(c) == id => {}
            (Some(_), _) => {
                return Err(unsafe_err(format!(
                    "export destination '{}' changed during export; aborting",
                    show(&out_real)
                )))
            }
            (None, Some(_)) => {
                return Err(unsafe_err(format!(
                    "export destination '{}' appeared during export; aborting",
                    show(&out_real)
                )))
            }
            (None, None) => {}
        }
        if lstat_opt(&old).map_err(io_fail)?.is_some() {
            return Err(unsafe_err(format!(
                "backup path '{}' already exists; aborting",
                show(&old)
            )));
        }

        if let Some(id) = out_id {
            fs::rename(&out_real, &old).map_err(io_fail)?;
            old_moved = true;
            // What we moved aside must be the directory we validated.
            match lstat_opt(&old).map_err(io_fail)? {
                Some(m) if is_real_dir(&m) && id_of(&m) == id => {}
                _ => {
                    return Err(unsafe_err(format!(
                        "export destination '{}' was replaced during the swap; aborting",
                        show(&out_real)
                    )))
                }
            }
        }
        fs::rename(&tmp, &out_real).map_err(io_fail)?;
        swapped = true;
        match lstat_opt(&out_real).map_err(io_fail)? {
            Some(n) if is_real_dir(&n) && id_of(&n) == tmp_id => Ok(()),
            _ => Err(unsafe_err(format!(
                "export destination '{}' is not the directory that was written; {}",
                show(&out_real),
                if old_moved {
                    format!("previous output preserved at '{}'", show(&old))
                } else {
                    "inspect it manually".to_string()
                }
            ))),
        }
    })();

    if let Err(e) = result {
        if !swapped {
            remove_if_same(&tmp, Some(tmp_id));
            if old_moved {
                // Roll back only if the slot is free and the backup is what we validated.
                let slot = lstat_opt(&out_real).ok().flatten();
                let bak = lstat_opt(&old).ok().flatten();
                if let (None, Some(b), Some(id)) = (slot, bak, out_id) {
                    if id_of(&b) == id {
                        let _ = fs::rename(&old, &out_real);
                    }
                }
            }
        }
        return Err(e);
    }
    if old_moved {
        // Best effort: the new output is in place. Only delete the exact directory we validated and moved.
        remove_if_same(&old, out_id);
    }
    if let Some(state) = state.as_deref_mut() {
        let mut stamps = stamps.into_inner().unwrap_or_else(|p| p.into_inner());
        stamps.sort_by_key(|(i, _)| *i);
        if stamps.len() == planned.len() {
            let files = planned
                .iter()
                .zip(stamps)
                .map(|(e, (_, stamp))| (e.path, e.digest, e.mode, stamp));
            exported(
                state,
                &out_real,
                tmp_id,
                parent_id,
                files,
                dirs_of(planned.iter().map(|e| e.path)),
            );
        }
    }
    Ok(ExportResult {
        out_dir: out_real,
        files: planned.len(),
        incremental: None,
    })
}

/// Write `data` as a single file at `out_file` (zip, tar, tgz, wbn, oci
/// archive). Same destination rules as [`export_directory`]: never the
/// filesystem root, the home directory, the build context or a parent of it;
/// never through a symlink; a directory is refused; an existing file outside
/// the context needs `force`. The bytes go to a temporary sibling renamed into
/// place after the parent and destination are re-validated; only that exact
/// temp file (by dev/ino) is ever deleted.
pub fn export_file(
    data: &[u8],
    out_file: &Path,
    options: &ExportOptions,
) -> Result<ExportFileResult> {
    let (out_real, context_real, st) = check_destination(out_file, options)?;
    if let Some(s) = &st {
        if s.is_dir() {
            return Err(SitefileError::plain(
                E_EXPORT,
                format!(
                    "export destination '{}' is a directory; an archive export needs a file path",
                    show(&out_real)
                ),
            ));
        }
        if !s.is_file() {
            return Err(SitefileError::plain(
                E_EXPORT,
                format!(
                    "export destination '{}' exists and is not a regular file",
                    show(&out_real)
                ),
            ));
        }
        if !is_inside(&out_real, &context_real) && !options.force {
            return Err(unsafe_err(format!(
                "export destination '{}' already exists outside the build context; pass --force to replace it",
                show(&out_real)
            )));
        }
    }

    let (parent, base) = split_parent(&out_real);
    let tmp = parent.join(format!(".{base}.sitefile-tmp-{}", random_hex(6)));
    mkdir_parent(&parent)?;
    let parent_id = verified_parent_id(&parent)?;
    let out_id = st.as_ref().map(id_of);
    // dev/ino alone can't tell a replaced file from the original: Linux often gives a file
    // created right after an unlink the same inode number. Size and the nanosecond ctime
    // can't survive a replacement (or any write) unchanged.
    let out_stamp = st.as_ref().map(replacement_stamp_of);

    let mut tmp_id: Option<FileId> = None;
    let mut renamed = false;
    let io_fail = |e: io::Error| {
        SitefileError::plain(
            E_EXPORT,
            format!("failed to export to '{}': {e}", show(&out_real)),
        )
    };
    let result = (|| -> Result<()> {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&tmp)
            .map_err(io_fail)?;
        let id = id_of(&f.metadata().map_err(io_fail)?);
        tmp_id = Some(id);
        f.write_all(data).map_err(io_fail)?;
        f.set_permissions(fs::Permissions::from_mode(0o644))
            .map_err(io_fail)?;
        drop(f);
        if let Some(hook) = &options.before_swap {
            hook();
        }

        assert_parent_unchanged(&parent, parent_id)?;
        match lstat_opt(&tmp).map_err(io_fail)? {
            Some(t) if is_real_file(&t) && id_of(&t) == id => {}
            _ => {
                return Err(unsafe_err(format!(
                    "temporary file '{}' changed during export; aborting",
                    show(&tmp)
                )))
            }
        }
        let cur = lstat_opt(&out_real).map_err(io_fail)?;
        match (out_id, &cur) {
            (Some(oid), Some(c))
                if is_real_file(c)
                    && id_of(c) == oid
                    && Some(replacement_stamp_of(c)) == out_stamp => {}
            (Some(_), _) => {
                return Err(unsafe_err(format!(
                    "export destination '{}' changed during export; aborting",
                    show(&out_real)
                )))
            }
            (None, Some(_)) => {
                return Err(unsafe_err(format!(
                    "export destination '{}' appeared during export; aborting",
                    show(&out_real)
                )))
            }
            (None, None) => {}
        }
        fs::rename(&tmp, &out_real).map_err(io_fail)?;
        renamed = true;
        match lstat_opt(&out_real).map_err(io_fail)? {
            Some(n) if is_real_file(&n) && id_of(&n) == id => Ok(()),
            _ => Err(unsafe_err(format!(
                "export destination '{}' is not the file that was written; inspect it manually",
                show(&out_real)
            ))),
        }
    })();
    if let Err(e) = result {
        if !renamed {
            if let Some(id) = tmp_id {
                remove_file_if_same(&tmp, id);
            }
        }
        return Err(e);
    }
    Ok(ExportFileResult {
        out_file: out_real,
        bytes: data.len(),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::tree::make_file;
    use std::os::unix::fs::symlink;

    /// A unique, real-path temp directory removed on drop.
    pub struct TmpDir(PathBuf);
    impl TmpDir {
        pub fn new(tag: &str) -> TmpDir {
            let p = std::env::temp_dir().join(format!(
                "sitefile-rs-{tag}-{}-{}",
                std::process::id(),
                random_hex(6)
            ));
            fs::create_dir_all(&p).unwrap();
            TmpDir(fs::canonicalize(&p).unwrap())
        }
        pub fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn tree(files: &[(&str, &str, u32)]) -> Tree {
        Tree::from_files(
            files
                .iter()
                .map(|(p, c, m)| (p.to_string(), make_file(c.as_bytes().to_vec(), "test", *m))),
        )
        .unwrap()
    }
    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }
    fn siblings_ok(parent: &Path) {
        let left: Vec<String> = names(parent)
            .into_iter()
            .filter(|n| matches_temp_pattern(n))
            .collect();
        assert!(left.is_empty(), "leftover temp siblings: {left:?}");
    }
    fn opts(ctx: &Path) -> ExportOptions {
        ExportOptions::new(ctx)
    }
    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }
    fn read(p: &Path) -> String {
        fs::read_to_string(p).unwrap()
    }
    fn hook(f: impl Fn() + Send + Sync + 'static) -> Option<BeforeSwap> {
        Some(Arc::new(f))
    }

    #[test]
    fn an_occupied_name_is_reported_by_artifact_path() {
        let e = occupied(io::Error::from(io::ErrorKind::AlreadyExists), "/a.html").unwrap();
        assert_eq!(e.code, E_EXPORT);
        assert!(e.detail.starts_with("cannot write '/a.html': the destination file system already has an entry by that name"), "{}", e.detail);
        assert!(occupied(io::Error::other("boom"), "/a").is_err());
    }

    #[test]
    fn writes_files_dirs_bytes_and_modes() {
        let d = TmpDir::new("export");
        let ctx = d.path();
        let out = ctx.join("dist");
        let t = tree(&[
            ("/index.html", "<h1>hi</h1>", 0o644),
            ("/a/b/c.txt", "deep", 0o644),
            ("/bin/run.sh", "#!/bin/sh\n", 0o755),
        ]);
        let res = export_directory(&t, &out, &opts(ctx)).unwrap();
        assert_eq!(
            res,
            ExportResult {
                out_dir: out.clone(),
                files: 3,
                incremental: None
            }
        );
        assert_eq!(read(&out.join("index.html")), "<h1>hi</h1>");
        assert_eq!(read(&out.join("a/b/c.txt")), "deep");
        assert_eq!(mode(&out.join("bin/run.sh")), 0o755);
        assert_eq!(mode(&out.join("index.html")), 0o644);
        siblings_ok(ctx);
    }

    #[test]
    fn creates_missing_parents_and_empty_tree() {
        let d = TmpDir::new("export");
        let out = d.path().join("deep/er/dist");
        let res = export_directory(&Tree::empty(), &out, &opts(d.path())).unwrap();
        assert_eq!(res.files, 0);
        assert!(names(&out).is_empty());
        siblings_ok(out.parent().unwrap());
    }

    #[test]
    fn replaces_existing_out_inside_context() {
        let d = TmpDir::new("export");
        let out = d.path().join("dist");
        fs::create_dir_all(out.join("stale")).unwrap();
        fs::write(out.join("stale/old.txt"), "old").unwrap();
        export_directory(&tree(&[("/new.txt", "new", 0o644)]), &out, &opts(d.path())).unwrap();
        assert_eq!(names(&out), ["new.txt"]);
        siblings_ok(d.path());
    }

    #[test]
    fn refuses_context_and_parents_of_it() {
        let d = TmpDir::new("export");
        let root = d.path();
        let ctx = root.join("project");
        fs::create_dir(&ctx).unwrap();
        fs::write(ctx.join("Sitefile"), "x").unwrap();
        let t = tree(&[("/a", "a", 0o644)]);
        let f = opts(&ctx).force(true);
        assert_eq!(
            export_directory(&t, &ctx, &f).unwrap_err().code,
            E_EXPORT_UNSAFE
        );
        assert_eq!(
            export_directory(&t, &ctx.join("."), &opts(&ctx))
                .unwrap_err()
                .code,
            E_EXPORT_UNSAFE
        );
        assert_eq!(
            export_directory(&t, root, &f).unwrap_err().code,
            E_EXPORT_UNSAFE
        );
        let link = root.join("link");
        symlink(&ctx, &link).unwrap();
        assert_eq!(
            export_directory(&t, &link.join("sub/.."), &f)
                .unwrap_err()
                .code,
            E_EXPORT_UNSAFE
        );
        assert_eq!(read(&ctx.join("Sitefile")), "x");
        siblings_ok(root);
    }

    #[test]
    fn refuses_root_and_home() {
        let d = TmpDir::new("export");
        let t = tree(&[("/a", "a", 0o644)]);
        let f = opts(d.path()).force(true);
        assert_eq!(
            export_directory(&t, Path::new("/"), &f).unwrap_err().code,
            E_EXPORT_UNSAFE
        );
        assert_eq!(
            export_directory(&t, &home_dir(), &f).unwrap_err().code,
            E_EXPORT_UNSAFE
        );
    }

    #[test]
    fn non_empty_outside_context_needs_force() {
        let d = TmpDir::new("export");
        let ctx = d.path().join("ctx");
        let out = d.path().join("out");
        fs::create_dir(&ctx).unwrap();
        fs::create_dir(&out).unwrap();
        fs::write(out.join("keep.txt"), "keep").unwrap();
        let t = tree(&[("/a", "a", 0o644)]);
        let e = export_directory(&t, &out, &opts(&ctx)).unwrap_err();
        assert_eq!(e.code, E_EXPORT_UNSAFE);
        assert!(e.detail.contains("--force"));
        assert_eq!(names(&out), ["keep.txt"]);
        export_directory(&t, &out, &opts(&ctx).force(true)).unwrap();
        assert_eq!(names(&out), ["a"]);
        siblings_ok(d.path());
    }

    #[test]
    fn empty_or_missing_outside_context_is_fine() {
        let d = TmpDir::new("export");
        let ctx = d.path().join("ctx");
        let out = d.path().join("out");
        fs::create_dir(&ctx).unwrap();
        fs::create_dir(&out).unwrap();
        export_directory(&tree(&[("/a", "a", 0o644)]), &out, &opts(&ctx)).unwrap();
        assert_eq!(names(&out), ["a"]);
        export_directory(
            &tree(&[("/b", "b", 0o644)]),
            &d.path().join("fresh"),
            &opts(&ctx),
        )
        .unwrap();
        assert_eq!(names(&d.path().join("fresh")), ["b"]);
    }

    #[test]
    fn out_that_is_a_file_or_symlink_fails() {
        let d = TmpDir::new("export");
        let ctx = d.path();
        let file = ctx.join("dist");
        fs::write(&file, "x").unwrap();
        let t = tree(&[("/a", "a", 0o644)]);
        let f = opts(ctx).force(true);
        assert_eq!(export_directory(&t, &file, &f).unwrap_err().code, E_EXPORT);
        assert_eq!(read(&file), "x");
        let target = ctx.join("target");
        fs::create_dir(&target).unwrap();
        let link = ctx.join("linked");
        symlink(&target, &link).unwrap();
        assert_eq!(export_directory(&t, &link, &f).unwrap_err().code, E_EXPORT);
        assert!(names(&target).is_empty());
        siblings_ok(ctx);
    }

    #[test]
    fn rejects_unsafe_artifact_paths_before_writing() {
        let d = TmpDir::new("export");
        let out = d.path().join("dist");
        for bad in ["/a\\b", "/a/../b", "relative", "/a//b", "/a/./b"] {
            let e = export_entries(&[(bad, b"x", 0o644, "")], &out, &opts(d.path())).unwrap_err();
            assert_eq!(e.code, E_EXPORT, "{bad}");
        }
        assert!(!out.exists());
        siblings_ok(d.path());
    }

    #[test]
    fn failed_write_leaves_previous_out_intact() {
        let d = TmpDir::new("export");
        let out = d.path().join("dist");
        fs::create_dir(&out).unwrap();
        fs::write(out.join("prev.txt"), "prev").unwrap();
        let long = format!("/{}", "n".repeat(300)); // exceeds NAME_MAX everywhere
        let t = tree(&[("/ok.txt", "ok", 0o644), (&long, "too long", 0o644)]);
        assert_eq!(
            export_directory(&t, &out, &opts(d.path()))
                .unwrap_err()
                .code,
            E_EXPORT
        );
        assert_eq!(names(&out), ["prev.txt"]);
        assert_eq!(read(&out.join("prev.txt")), "prev");
        siblings_ok(d.path());
    }

    #[test]
    fn temp_names() {
        assert!(is_exporter_temp_name(".dist.sitefile-tmp-0a1b2c", "dist"));
        assert!(is_exporter_temp_name(".dist.sitefile-old-0a1b2c", "dist"));
        assert!(!is_exporter_temp_name(".dist.sitefile-tmp-0a1b2c", "out"));
        assert!(!is_exporter_temp_name("dist", "dist"));
        assert!(!is_exporter_temp_name(".dist.sitefile-new-0a1b2c", "dist"));
        assert!(!matches_temp_pattern("..sitefile-tmp-00"));
        assert!(matches_temp_pattern(".a.sitefile-tmp-x.sitefile-old-00"));
        assert_eq!(random_hex(6).len(), 12);
    }

    fn victim(root: &Path) -> (PathBuf, PathBuf) {
        let dir = root.join("victim");
        fs::create_dir(&dir).unwrap();
        let file = dir.join("precious.txt");
        fs::write(&file, "keep me").unwrap();
        (dir, file)
    }
    fn victim_untouched(v: &(PathBuf, PathBuf)) {
        assert_eq!(names(&v.0), ["precious.txt"]);
        assert_eq!(read(&v.1), "keep me");
    }

    #[test]
    fn race_out_replaced_by_symlink() {
        let d = TmpDir::new("export");
        let ctx = d.path().join("ctx");
        fs::create_dir(&ctx).unwrap();
        let out = ctx.join("dist");
        fs::create_dir(&out).unwrap();
        fs::write(out.join("old.txt"), "old").unwrap();
        let v = victim(d.path());
        let (o, vd) = (out.clone(), v.0.clone());
        let mut op = opts(&ctx);
        op.before_swap = hook(move || {
            fs::remove_dir_all(&o).unwrap();
            symlink(&vd, &o).unwrap();
        });
        assert_eq!(
            export_directory(&tree(&[("/new.txt", "new", 0o644)]), &out, &op)
                .unwrap_err()
                .code,
            E_EXPORT_UNSAFE
        );
        victim_untouched(&v);
        assert!(fs::symlink_metadata(&out).unwrap().file_type().is_symlink());
        siblings_ok(&ctx);
    }

    #[test]
    fn race_parent_replaced_by_symlink() {
        let d = TmpDir::new("export");
        let ctx = d.path().join("ctx");
        let sub = ctx.join("sub");
        fs::create_dir_all(&sub).unwrap();
        let out = sub.join("dist");
        fs::create_dir(&out).unwrap();
        fs::write(out.join("old.txt"), "old").unwrap();
        let v = victim(d.path());
        let aside = d.path().join("sub-aside");
        let (s, a, vd) = (sub.clone(), aside.clone(), v.0.clone());
        let mut op = opts(&ctx);
        op.before_swap = hook(move || {
            fs::rename(&s, &a).unwrap();
            symlink(&vd, &s).unwrap();
        });
        assert_eq!(
            export_directory(&tree(&[("/new.txt", "new", 0o644)]), &out, &op)
                .unwrap_err()
                .code,
            E_EXPORT_UNSAFE
        );
        victim_untouched(&v);
        siblings_ok(&v.0);
        assert_eq!(read(&aside.join("dist/old.txt")), "old");
    }

    #[test]
    fn race_out_replaced_by_other_directory() {
        let d = TmpDir::new("export");
        let ctx = d.path().join("ctx");
        fs::create_dir(&ctx).unwrap();
        let out = ctx.join("dist");
        fs::create_dir(&out).unwrap();
        fs::write(out.join("old.txt"), "old").unwrap();
        let keep = d.path().join("original-aside");
        let (o, k) = (out.clone(), keep.clone());
        let mut op = opts(&ctx);
        op.before_swap = hook(move || {
            fs::rename(&o, &k).unwrap();
            fs::create_dir(&o).unwrap();
            fs::write(o.join("attacker.txt"), "theirs").unwrap();
        });
        assert_eq!(
            export_directory(&tree(&[("/new.txt", "new", 0o644)]), &out, &op)
                .unwrap_err()
                .code,
            E_EXPORT_UNSAFE
        );
        assert_eq!(names(&out), ["attacker.txt"]);
        assert_eq!(read(&keep.join("old.txt")), "old");
        siblings_ok(&ctx);
    }

    #[test]
    fn race_out_appearing() {
        let d = TmpDir::new("export");
        let ctx = d.path().join("ctx");
        fs::create_dir(&ctx).unwrap();
        let out = ctx.join("dist");
        let o = out.clone();
        let mut op = opts(&ctx);
        op.before_swap = hook(move || {
            fs::create_dir(&o).unwrap();
            fs::write(o.join("attacker.txt"), "theirs").unwrap();
        });
        assert_eq!(
            export_directory(&tree(&[("/new.txt", "new", 0o644)]), &out, &op)
                .unwrap_err()
                .code,
            E_EXPORT_UNSAFE
        );
        assert_eq!(names(&out), ["attacker.txt"]);
        siblings_ok(&ctx);
    }

    #[test]
    fn noop_hook_still_exports() {
        let d = TmpDir::new("export");
        let out = d.path().join("dist");
        fs::create_dir(&out).unwrap();
        fs::write(out.join("old.txt"), "old").unwrap();
        let called = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = called.clone();
        let mut op = opts(d.path());
        op.before_swap = hook(move || {
            c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        export_directory(&tree(&[("/new.txt", "new", 0o644)]), &out, &op).unwrap();
        assert_eq!(called.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(names(&out), ["new.txt"]);
        siblings_ok(d.path());
    }

    // ------------------------------------------------------------ export_file

    #[test]
    fn file_writes_and_replaces_inside_context() {
        let d = TmpDir::new("fmt");
        let ctx = d.path();
        let out = ctx.join("dist.zip");
        let r = export_file(b"one", &out, &opts(ctx)).unwrap();
        assert_eq!(
            r,
            ExportFileResult {
                out_file: out.clone(),
                bytes: 3
            }
        );
        export_file(b"two", &out, &opts(ctx)).unwrap();
        assert_eq!(read(&out), "two");
        assert_eq!(mode(&out), 0o644);
        assert_eq!(names(ctx), ["dist.zip"]);
    }

    #[test]
    fn file_outside_context_needs_force() {
        let a = TmpDir::new("fmt");
        let b = TmpDir::new("fmt");
        let out = b.path().join("a.zip");
        fs::write(&out, "precious").unwrap();
        assert_eq!(
            export_file(b"new", &out, &opts(a.path())).unwrap_err().code,
            E_EXPORT_UNSAFE
        );
        assert_eq!(read(&out), "precious");
        export_file(b"new", &out, &opts(a.path()).force(true)).unwrap();
        assert_eq!(read(&out), "new");
        export_file(b"x", &b.path().join("fresh/b.zip"), &opts(a.path())).unwrap();
    }

    #[test]
    fn file_refusals() {
        let a = TmpDir::new("fmt");
        let b = TmpDir::new("fmt");
        let (ctx, other) = (a.path(), b.path());
        let f = opts(ctx).force(true);
        fs::create_dir(other.join("adir")).unwrap();
        assert_eq!(
            export_file(b"x", &other.join("adir"), &f).unwrap_err().code,
            E_EXPORT
        );
        fs::write(other.join("real"), "r").unwrap();
        symlink(other.join("real"), other.join("link")).unwrap();
        assert_eq!(
            export_file(b"x", &other.join("link"), &f).unwrap_err().code,
            E_EXPORT
        );
        assert_eq!(read(&other.join("real")), "r");
        assert_eq!(
            export_file(b"x", ctx, &opts(ctx)).unwrap_err().code,
            E_EXPORT_UNSAFE
        );
        assert_eq!(
            export_file(b"x", ctx.parent().unwrap(), &f)
                .unwrap_err()
                .code,
            E_EXPORT_UNSAFE
        );
        assert_eq!(
            export_file(b"x", Path::new("/"), &f).unwrap_err().code,
            E_EXPORT_UNSAFE
        );
        assert_eq!(
            export_file(b"x", &home_dir(), &f).unwrap_err().code,
            E_EXPORT_UNSAFE
        );
        assert_eq!(names(other), ["adir", "link", "real"]);
    }

    #[test]
    fn file_destination_changed_during_write() {
        let d = TmpDir::new("fmt");
        let ctx = d.path().to_path_buf();
        let out = ctx.join("a.zip");
        fs::write(&out, "old").unwrap();
        let o = out.clone();
        let mut op = opts(&ctx);
        op.before_swap = hook(move || {
            fs::remove_file(&o).unwrap();
            fs::write(&o, "attacker").unwrap();
        });
        assert_eq!(
            export_file(b"new", &out, &op).unwrap_err().code,
            E_EXPORT_UNSAFE
        );
        assert_eq!(read(&out), "attacker");
        assert_eq!(names(&ctx), ["a.zip"]);

        let fresh = ctx.join("b.zip");
        let fr = fresh.clone();
        op.before_swap = hook(move || fs::write(&fr, "appeared").unwrap());
        assert_eq!(
            export_file(b"new", &fresh, &op).unwrap_err().code,
            E_EXPORT_UNSAFE
        );
        assert_eq!(read(&fresh), "appeared");
        assert_eq!(names(&ctx), ["a.zip", "b.zip"]);
    }

    #[test]
    fn resolve_is_lexical() {
        assert_eq!(
            resolve_path(Path::new("/a/b/../c/./d/")),
            PathBuf::from("/a/c/d")
        );
        assert_eq!(resolve_path(Path::new("/..")), PathBuf::from("/"));
        assert!(resolve_path(Path::new("rel")).is_absolute());
    }
}

//! Incremental export in watch mode (port of the "incremental export" section of
//! `src/exporter.ts`; the comment there is the full description).
//!
//! A watch session keeps an [`ExportState`]: the destination's identity and, for every file
//! it exported, the tree digest and mode plus the file's lstat stamp (dev, ino, size, mtime,
//! ctime, mode) as it was right after Sitefile wrote it. The next export first checks that
//! the destination still holds exactly that (same directory and parent, every recorded
//! directory and file and nothing else, each file with its stamp); if so it stages the
//! changed files in a temporary sibling, then unlinks deleted files, removes emptied
//! directories, creates new ones and renames each staged file over its path, re-checking
//! what each step replaces. Anything unexpected (or more than half of the files changing)
//! falls back to the full write-and-swap in `exporter.rs`.

use super::exporter::{
    assert_parent_unchanged, id_of, io_threads, is_real_dir, is_real_file, lstat_opt, random_hex,
    remove_if_same, show, split_parent, unsafe_err, ExportOptions, ExportResult, FileId, Planned,
};
use crate::error::Result;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// What an incremental export did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    pub written: usize,
    pub removed: usize,
}

/// What a watch session's last directory export left on disk.
#[derive(Default)]
pub struct ExportState {
    snapshot: Option<Snapshot>,
}

/// Shared by a watch session's builds (held by its memo).
pub type SharedExportState = Arc<Mutex<ExportState>>;

impl ExportState {
    /// Forget the last export: the next one rewrites everything.
    pub fn clear(&mut self) {
        self.snapshot = None;
    }
}

/// Lock a shared state and forget its last export.
pub fn clear_shared(state: &SharedExportState) {
    state.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Stamp {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    mode: u32,
}

pub(super) fn stamp_of(m: &fs::Metadata) -> Stamp {
    Stamp {
        dev: m.dev(),
        ino: m.ino(),
        size: m.size(),
        mtime: (m.mtime(), m.mtime_nsec()),
        ctime: (m.ctime(), m.ctime_nsec()),
        mode: m.mode(),
    }
}

fn same_stamp(m: Option<&fs::Metadata>, s: &Stamp) -> bool {
    matches!(m, Some(m) if is_real_file(m) && stamp_of(m) == *s)
}

struct Recorded {
    digest: String,
    mode: u32,
    stamp: Stamp,
}

struct Snapshot {
    out_real: PathBuf,
    out_id: FileId,
    parent_id: FileId,
    /// Artifact path (`/a/b.html`) → what was written there.
    files: HashMap<String, Recorded>,
    /// Every directory below the destination (`/a`, `/a/b`).
    dirs: HashSet<String>,
}

/// Every directory that holds one of `paths`, below the root.
pub(super) fn dirs_of<'a>(paths: impl Iterator<Item = &'a str>) -> HashSet<String> {
    let mut dirs = HashSet::new();
    for p in paths {
        for (i, _) in p.match_indices('/').skip(1) {
            dirs.insert(p[..i].to_string());
        }
    }
    dirs
}

/// Record a full export: (path, digest, mode, stamp) for every file.
pub(super) fn exported<'a>(
    state: &mut ExportState,
    out_real: &Path,
    out_id: FileId,
    parent_id: FileId,
    files: impl Iterator<Item = (&'a str, &'a str, u32, Stamp)>,
    dirs: HashSet<String>,
) {
    let files = files
        .map(|(p, digest, mode, stamp)| {
            (
                p.to_string(),
                Recorded {
                    digest: digest.to_string(),
                    mode,
                    stamp,
                },
            )
        })
        .collect();
    state.snapshot = Some(Snapshot {
        out_real: out_real.to_path_buf(),
        out_id,
        parent_id,
        files,
        dirs,
    });
}

fn depth(p: &str) -> usize {
    p.bytes().filter(|&b| b == b'/').count()
}

fn at(out: &Path, p: &str) -> PathBuf {
    let mut t = out.to_path_buf();
    for s in p[1..].split('/') {
        t.push(s);
    }
    t
}

/// True if `out` holds exactly the recorded directories and files, each file as recorded.
fn matches_snapshot(out: &Path, snap: &Snapshot) -> bool {
    let mut files: Vec<(PathBuf, &Stamp)> = Vec::with_capacity(snap.files.len());
    let mut dirs = 0usize;
    let mut pending = vec![String::new()];
    while let Some(rel) = pending.pop() {
        let Ok(rd) = fs::read_dir(if rel.is_empty() {
            out.to_path_buf()
        } else {
            at(out, &rel)
        }) else {
            return false;
        };
        for e in rd {
            let Ok(e) = e else { return false };
            let Some(name) = e.file_name().to_str().map(str::to_string) else {
                return false;
            };
            let p = format!("{rel}/{name}");
            let Ok(t) = e.file_type() else { return false };
            if t.is_dir() {
                if !snap.dirs.contains(&p) {
                    return false;
                }
                dirs += 1;
                pending.push(p);
            } else if t.is_file() {
                let Some(r) = snap.files.get(&p) else {
                    return false;
                };
                files.push((e.path(), &r.stamp));
            } else {
                return false; // a symlink, socket, ...: never ours
            }
        }
    }
    if dirs != snap.dirs.len() || files.len() != snap.files.len() {
        return false;
    }
    let threads = io_threads(files.len() / 256 + 1);
    let chunk = files.len().div_ceil(threads).max(1);
    std::thread::scope(|sc| {
        let parts: Vec<_> = files
            .chunks(chunk)
            .map(|part| {
                sc.spawn(move || {
                    part.iter()
                        .all(|(p, s)| same_stamp(lstat_opt(p).ok().flatten().as_ref(), s))
                })
            })
            .collect();
        parts.into_iter().all(|h| h.join().unwrap_or(false))
    })
}

/// Apply only the changes since the recorded export. None asks for the full swap: the
/// destination is then either untouched or, if applying failed part way, about to be
/// replaced whole.
pub(super) fn try_incremental(
    planned: &[Planned],
    out_real: &Path,
    st: Option<&fs::Metadata>,
    state: &mut ExportState,
    options: &ExportOptions,
) -> Option<ExportResult> {
    let snap = state.snapshot.take()?; // recorded again only after a successful export
    let st = st?;
    if snap.out_real != out_real || !is_real_dir(st) || id_of(st) != snap.out_id {
        return None;
    }
    let next: HashSet<&str> = planned.iter().map(|e| e.path).collect();
    let writes: Vec<&Planned> = planned
        .iter()
        .filter(|e| match snap.files.get(e.path) {
            Some(r) => r.digest != e.digest || r.mode != e.mode,
            None => true,
        })
        .collect();
    let mut deletes: Vec<String> = snap
        .files
        .keys()
        .filter(|p| !next.contains(p.as_str()))
        .cloned()
        .collect();
    deletes.sort_unstable();
    if (writes.len() + deletes.len()) * 2 > planned.len().max(snap.files.len()) {
        return None;
    }
    let next_dirs = dirs_of(planned.iter().map(|e| e.path));
    let mut remove_dirs: Vec<&str> = snap
        .dirs
        .iter()
        .map(String::as_str)
        .filter(|d| !next_dirs.contains(*d))
        .collect();
    remove_dirs.sort_by(|a, b| depth(b).cmp(&depth(a)).then(a.cmp(b)));
    let mut make_dirs: Vec<&str> = next_dirs
        .iter()
        .map(String::as_str)
        .filter(|d| !snap.dirs.contains(*d))
        .collect();
    make_dirs.sort_by(|a, b| depth(a).cmp(&depth(b)).then(a.cmp(b)));
    let (parent, base) = split_parent(out_real);

    assert_parent_unchanged(&parent, snap.parent_id).ok()?;
    if !matches_snapshot(out_real, &snap) {
        return None;
    }
    // A new path must not exist yet, except as the very file or directory it replaces;
    // anything else is another spelling of an existing entry (case folding, Unicode
    // normalization): the full export reports that as a clean build does.
    let going: HashSet<&str> = deletes
        .iter()
        .map(String::as_str)
        .chain(remove_dirs.iter().copied())
        .collect();
    for p in writes
        .iter()
        .map(|e| e.path)
        .filter(|p| !snap.files.contains_key(*p))
        .chain(make_dirs.iter().copied())
    {
        if !matches!(lstat_opt(&at(out_real, p)), Ok(None)) && !going.contains(p) {
            return None;
        }
    }

    // Stage the new contents next to the destination.
    let tmp = parent.join(format!(".{base}.sitefile-tmp-{}", random_hex(6)));
    if fs::create_dir(&tmp).is_err() {
        return None;
    }
    let tmp_id = match lstat_opt(&tmp) {
        Ok(Some(t)) if is_real_dir(&t) => id_of(&t),
        _ => {
            remove_if_same(&tmp, None);
            return None;
        }
    };
    let stage = || -> std::io::Result<Vec<(&Planned, PathBuf, FileId)>> {
        let mut staged = Vec::with_capacity(writes.len());
        for (i, e) in writes.iter().enumerate() {
            let file = tmp.join(i.to_string());
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&file)?;
            f.write_all(e.bytes)?;
            f.set_permissions(fs::Permissions::from_mode(e.mode))?;
            let id = id_of(&f.metadata()?);
            staged.push((*e, file, id));
        }
        Ok(staged)
    };
    let staged = match stage() {
        Ok(s) => s,
        Err(_) => {
            remove_if_same(&tmp, Some(tmp_id));
            return None;
        }
    };
    if let Some(hook) = &options.before_apply {
        hook();
    }

    // Apply.
    let Snapshot {
        out_id,
        parent_id,
        files: mut recorded,
        ..
    } = snap;
    let apply = |recorded: &mut HashMap<String, Recorded>| -> Result<()> {
        let io = |e: std::io::Error| {
            unsafe_err(format!("failed to export to '{}': {e}", show(out_real)))
        };
        assert_parent_unchanged(&parent, parent_id)?;
        match lstat_opt(out_real).map_err(io)? {
            Some(m) if is_real_dir(&m) && id_of(&m) == out_id => {}
            _ => {
                return Err(unsafe_err(format!(
                    "export destination '{}' changed during export; aborting",
                    show(out_real)
                )))
            }
        }
        for p in &deletes {
            let t = at(out_real, p);
            if !same_stamp(lstat_opt(&t).map_err(io)?.as_ref(), &recorded[p].stamp) {
                return Err(unsafe_err(format!("'{}' changed during export", show(&t))));
            }
            fs::remove_file(&t).map_err(io)?;
            recorded.remove(p);
        }
        for d in &remove_dirs {
            fs::remove_dir(at(out_real, d)).map_err(io)?; // fails unless emptied
        }
        for d in &make_dirs {
            let t = at(out_real, d);
            fs::create_dir(&t).map_err(io)?;
            match lstat_opt(&t).map_err(io)? {
                Some(m) if is_real_dir(&m) => {}
                _ => return Err(unsafe_err(format!("'{}' is not a directory", show(&t)))),
            }
        }
        for (e, file, id) in &staged {
            let t = at(out_real, e.path);
            let cur = lstat_opt(&t).map_err(io)?;
            let ok = match recorded.get(e.path) {
                Some(r) => same_stamp(cur.as_ref(), &r.stamp),
                None => cur.is_none(),
            };
            if !ok {
                return Err(unsafe_err(format!("'{}' changed during export", show(&t))));
            }
            fs::rename(file, &t).map_err(io)?;
            let now = match lstat_opt(&t).map_err(io)? {
                Some(m) if is_real_file(&m) && id_of(&m) == *id => m,
                _ => {
                    return Err(unsafe_err(format!(
                        "'{}' is not the file that was written",
                        show(&t)
                    )))
                }
            };
            recorded.insert(
                e.path.to_string(),
                Recorded {
                    digest: e.digest.to_string(),
                    mode: e.mode,
                    stamp: stamp_of(&now),
                },
            );
        }
        Ok(())
    };
    let ok = apply(&mut recorded).is_ok();
    remove_if_same(&tmp, Some(tmp_id));
    if !ok {
        return None; // the full swap replaces whatever was applied
    }
    state.snapshot = Some(Snapshot {
        out_real: out_real.to_path_buf(),
        out_id,
        parent_id,
        files: recorded,
        dirs: next_dirs,
    });
    Some(ExportResult {
        out_dir: out_real.to_path_buf(),
        files: planned.len(),
        incremental: Some(Applied {
            written: writes.len(),
            removed: deletes.len(),
        }),
    })
}

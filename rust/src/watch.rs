//! Watch mode (port of `src/watch.ts` and `src/cli/watch.ts`): `build --watch`
//! and `serve --watch` rebuild when the context, the Sitefile or a
//! `--build-arg-file` changes.
//!
//! The TypeScript version uses `fs.watch`; std has no file notification API, so
//! this one polls: every [`POLL`] it walks the watched paths (without following
//! symlinks) and compares size, mtime, ctime, inode and type. A change starts a
//! rebuild once nothing has changed for [`DEBOUNCE`]; changes made during a
//! build show up in the next scan and coalesce into one follow-up build. The
//! same paths are ignored and the same lines printed as in the TypeScript
//! version.

use crate::cli::common::{err, out};
use crate::error::{ErrorCode::*, Result, SitefileError};
use crate::export::exporter::{matches_temp_pattern, resolve_path};
use crate::store::resolve_store_home;
use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Quiet period after the last change before a rebuild starts.
pub const DEBOUNCE: Duration = Duration::from_millis(100);
/// How often the watched paths are scanned.
pub const POLL: Duration = Duration::from_millis(100);
/// Default interval of `--poll`.
pub const DEFAULT_POLL: Duration = Duration::from_millis(1000);

/// `--poll` / `--poll-interval <ms>` (the interval implies polling): the scan interval, or `None`
/// for the default ([`POLL`]). Same rules and message as `pollOption` in src/cli/watch.ts.
pub fn poll_option(poll: bool, interval: Option<&str>) -> Result<Option<Duration>> {
    let Some(i) = interval else {
        return Ok(poll.then_some(DEFAULT_POLL));
    };
    match i.parse::<u64>() {
        Ok(n)
            if n >= 10
                && !i.is_empty()
                && i.len() <= 9
                && i.bytes().all(|b| b.is_ascii_digit()) =>
        {
            Ok(Some(Duration::from_millis(n)))
        }
        _ => Err(SitefileError::plain(
            E_USAGE,
            format!(
                "--poll-interval expects a whole number of milliseconds (10 or more), got '{i}'"
            ),
        )),
    }
}

const MSG_CHANGE: &str = "Change detected, rebuilding\n";
const MSG_FAILED: &str = "Build failed; keeping the last good output\n";
const MSG_CHANGE_PLAIN: &str = "Change detected, re-reading\n";
const MSG_FAILED_PLAIN: &str = "Read failed; serving the last good copy\n";

/// `p` is `dir` or inside it (both absolute).
pub fn is_within(p: &Path, dir: &Path) -> bool {
    p.starts_with(dir)
}

/// Paths a watcher never reacts to: anything under `.git`, the exporter's temp
/// and backup siblings, and any of `ignored` (the output, the store, Sitefile.lock).
pub fn is_ignored_change(abs: &Path, roots: &[PathBuf], ignored: &[PathBuf]) -> bool {
    if ignored.iter().any(|q| is_within(abs, q)) {
        return true;
    }
    let Some(root) = roots.iter().find(|r| is_within(abs, r)) else {
        return false;
    };
    abs.strip_prefix(root)
        .map(|rel| {
            rel.iter()
                .any(|seg| seg == ".git" || seg.to_str().is_some_and(matches_temp_pattern))
        })
        .unwrap_or(false)
}

/// Canonical path of `p`, else of its parent joined with its name, else `p` made absolute.
pub fn resolve_real(p: &Path) -> PathBuf {
    let abs = resolve_path(p);
    if let Ok(r) = std::fs::canonicalize(&abs) {
        return r;
    }
    match (abs.parent().map(std::fs::canonicalize), abs.file_name()) {
        (Some(Ok(parent)), Some(name)) => parent.join(name),
        _ => abs,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// What a scan compares per path: type, size, mtime, ctime and inode.
pub struct Stamp {
    kind: u32,
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    ino: u64,
}

fn stamp(m: &std::fs::Metadata) -> Stamp {
    Stamp {
        kind: m.mode() & 0o170000,
        len: m.len(),
        mtime: (m.mtime(), m.mtime_nsec()),
        ctime: (m.ctime(), m.ctime_nsec()),
        ino: m.ino(),
    }
}

pub type Snapshot = BTreeMap<PathBuf, Option<Stamp>>;

/// What the watcher looks at, and what it skips.
pub struct Watched {
    /// Directories scanned recursively (the build context). Canonical paths.
    pub dirs: Vec<PathBuf>,
    /// Single files (the Sitefile, `--build-arg-file` files); a missing one is recorded as missing.
    pub files: Vec<PathBuf>,
    pub ignored: Vec<PathBuf>,
}

impl Watched {
    fn ignores(&self, p: &Path) -> bool {
        is_ignored_change(p, &self.dirs, &self.ignored)
    }

    fn walk(&self, dir: &Path, snap: &mut Snapshot) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if self.ignores(&p) {
                continue;
            }
            let Ok(m) = std::fs::symlink_metadata(&p) else {
                continue;
            };
            let is_dir = m.is_dir();
            snap.insert(p.clone(), Some(stamp(&m)));
            if is_dir {
                self.walk(&p, snap);
            }
        }
    }

    /// A stat snapshot of every watched path; two snapshots differ when something changed.
    pub fn scan(&self) -> Snapshot {
        let mut snap = Snapshot::new();
        for d in &self.dirs {
            self.walk(d, &mut snap);
        }
        for f in &self.files {
            if !snap.contains_key(f) && !self.ignores(f) {
                snap.insert(f.clone(), std::fs::metadata(f).ok().map(|m| stamp(&m)));
            }
        }
        snap
    }
}

/// What one build reports to the loop.
pub struct BuildOutcome {
    /// CLI exit code: 0 ok, 1 build error, 2 usage error (already printed).
    pub code: i32,
    /// Paths the build writes (the export destination); never inputs.
    pub wrote: Vec<String>,
}

pub struct WatchOptions<'a> {
    /// The build context, as given (shown in messages).
    pub context: &'a str,
    /// `-f`: the Sitefile path (default `<context>/Sitefile`).
    pub sitefile: Option<&'a str>,
    /// `--build-arg-file` files (re-read by every build).
    pub arg_files: &'a [String],
    /// `--poll`: scan every this long instead of every [`POLL`]. (This implementation always polls;
    /// the flag only slows the scans down, for network shares where each stat is slow.)
    pub poll: Option<Duration>,
    /// `serve --watch ./dir` without a Sitefile: the directory is re-read, not built (other messages).
    pub plain: bool,
    /// Asked between scans: true builds again now, as if a file had changed (`serve --watch`, key `r`).
    pub rebuild: Option<&'a dyn Fn() -> bool>,
}

/// Build once, then rebuild whenever a watched path changes, until `stop()`.
/// `on_started` runs after the first build, before the "Watching" line (serve
/// starts listening there). Returns the exit code: 0 when stopped, 2 when the
/// first build is a usage error, 1 when `on_started` fails.
pub fn watch_loop(
    o: &WatchOptions<'_>,
    build: &mut dyn FnMut() -> BuildOutcome,
    on_started: &mut dyn FnMut() -> Result<()>,
    stop: &dyn Fn() -> bool,
) -> Result<i32> {
    if !std::fs::metadata(o.context)
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
        return Err(SitefileError::plain(
            E_NOT_FOUND,
            format!("'{}' is not a directory", o.context),
        ));
    }
    let root = std::fs::canonicalize(o.context)
        .map_err(|e| SitefileError::plain(E_NOT_FOUND, format!("'{}': {e}", o.context)))?;
    let sitefile = resolve_real(&match o.sitefile {
        Some(f) => PathBuf::from(f),
        None => Path::new(o.context).join("Sitefile"),
    });
    let lock = sitefile
        .parent()
        .unwrap_or(Path::new("/"))
        .join("Sitefile.lock");
    let mut files = vec![sitefile];
    files.extend(o.arg_files.iter().map(|f| resolve_real(Path::new(f))));
    // The build itself writes the export, Sitefile.lock and (maybe) the store; none of that is input.
    let mut w = Watched {
        dirs: vec![root],
        files,
        ignored: vec![resolve_real(&resolve_store_home()), lock],
    };

    let mut last = w.scan();
    let mut run = |w: &mut Watched, last: &mut Snapshot| -> i32 {
        let r = build();
        for p in &r.wrote {
            for q in [resolve_path(Path::new(p)), resolve_real(Path::new(p))] {
                if !w.ignored.contains(&q) {
                    w.ignored.push(q);
                }
            }
        }
        // what the build wrote was in the pre-build snapshot only if it was not yet ignored
        last.retain(|p, _| !w.ignores(p));
        r.code
    };
    let code = run(&mut w, &mut last);
    if code == 2 {
        return Ok(2);
    }
    if let Err(e) = on_started() {
        return Ok(crate::cli::common::report(&e));
    }
    out(&format!(
        "Watching {} for changes (Ctrl-C to stop)\n",
        o.context
    ));

    let mut dirty_since: Option<Instant> = None;
    while !stop() {
        std::thread::sleep(o.poll.unwrap_or(POLL));
        if o.rebuild.is_some_and(|f| f()) {
            dirty_since = Some(Instant::now() - DEBOUNCE);
        }
        let snap = w.scan();
        if snap != last {
            last = snap;
            dirty_since = Some(Instant::now()); // (re)start the debounce
            continue;
        }
        if dirty_since.is_some_and(|t| t.elapsed() >= DEBOUNCE) {
            dirty_since = None;
            out(if o.plain {
                MSG_CHANGE_PLAIN
            } else {
                MSG_CHANGE
            });
            if run(&mut w, &mut last) != 0 {
                err(if o.plain {
                    MSG_FAILED_PLAIN
                } else {
                    MSG_FAILED
                });
            }
            // a change during the build differs from `last` in the next scan: one follow-up build
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignored_paths() {
        let root = PathBuf::from("/ctx");
        let ignored = vec![root.join("dist"), PathBuf::from("/home/.sitefile")];
        let ig = |p: &str| is_ignored_change(&root.join(p), std::slice::from_ref(&root), &ignored);
        assert!(!ig("public/index.html"));
        assert!(!ig("Sitefile"));
        assert!(ig("dist"));
        assert!(ig("dist/index.html"));
        assert!(!ig("distillery/x"), "prefix of a name is not inside it");
        assert!(ig(".git/index"));
        assert!(ig("sub/.git/HEAD"));
        assert!(ig(".dist.sitefile-tmp-0a1b2c/index.html"));
        assert!(ig(".dist.sitefile-old-ff"));
        assert!(is_ignored_change(
            Path::new("/home/.sitefile/cache/x"),
            &[root],
            &ignored
        ));
    }

    #[test]
    fn poll_option_rules() {
        assert_eq!(poll_option(false, None).unwrap(), None);
        assert_eq!(poll_option(true, None).unwrap(), Some(DEFAULT_POLL));
        assert_eq!(
            poll_option(false, Some("250")).unwrap(),
            Some(Duration::from_millis(250))
        );
        for bad in ["5", "1.5", "x", "-1", "", "+50"] {
            assert!(poll_option(true, Some(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn scan_sees_changes_and_skips_ignored() {
        let dir = std::env::temp_dir().join(format!("sitefile-watch-rs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dist")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "1").unwrap();
        let w = Watched {
            dirs: vec![dir.clone()],
            files: vec![dir.join("missing.env")],
            ignored: vec![dir.join("dist")],
        };
        let s1 = w.scan();
        assert!(s1.contains_key(&dir.join("a.txt")));
        assert_eq!(s1.get(&dir.join("missing.env")), Some(&None));
        std::fs::write(dir.join("dist").join("x.html"), "out").unwrap();
        assert_eq!(w.scan(), s1, "writes under an ignored path are not changes");
        std::fs::write(dir.join("a.txt"), "22").unwrap();
        assert_ne!(w.scan(), s1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

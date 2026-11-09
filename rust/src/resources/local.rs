//! The local build context (port of `src/resources/local.ts`). Every read is
//! confined to the context root: real paths are resolved and must stay inside
//! it. POSIX-only, like the TypeScript version.

use crate::digest::sha256;
use crate::error::{ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::paths::nfc;
use crate::platform::{Meta, MetadataExt, PermissionsExt};
use crate::resources::ignore::IgnoreRules;
use crate::resources::types::{Provenance, Resource};
use crate::tree::{make_file, ConflictPolicy, Node, Tree};
use std::collections::HashSet;
use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};

pub const SITEFILEIGNORE: &str = ".sitefileignore";

#[derive(Debug, Clone)]
pub struct LocalContextOptions {
    /// Context-relative POSIX paths skipped while walking directories. `None` means `[".git"]`.
    pub ignore: Option<Vec<String>>,
    /// Read `.sitefileignore` from the context root (default true). Applies to directory walks only.
    pub sitefileignore: bool,
    /// Permit regular files with more than one hard link (default false). realpath cannot
    /// tell whether a hard link points outside the context, so by default such files are
    /// refused with `E_HARDLINK`. Allowing them means you assert they are safe.
    pub allow_hardlinks: bool,
}

impl Default for LocalContextOptions {
    fn default() -> Self {
        LocalContextOptions {
            ignore: None,
            sitefileignore: true,
            allow_hardlinks: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
}

#[derive(Debug, Clone)]
pub struct LocalContext {
    /// The real (symlink-free) path of the context directory.
    pub root: PathBuf,
    ignore: Vec<String>,
    rules: IgnoreRules,
    allow_hardlinks: bool,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
const ELOOP: i32 = 40;
#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
const ELOOP: i32 = 62;
/// `ERROR_CANT_RESOLVE_FILENAME`, what `canonicalize` reports for a symlink loop on Windows.
#[cfg(windows)]
const ELOOP: i32 = 1921;

fn display(rel: &str) -> String {
    if rel.is_empty() {
        ".".into()
    } else {
        format!("./{rel}")
    }
}

fn normalization_collision(rel: &str, span: Option<&SourceSpan>) -> SitefileError {
    SitefileError::new(
        E_PATH_COLLISION,
        format!(
            "'{}' names more than one file in the build context: their names differ only in Unicode normalization (NFC/NFD); rename one",
            display(rel)
        ),
        span.cloned(),
    )
}

/// `posix.normalize(p)` for a relative or absolute POSIX path.
fn posix_normalize(p: &str) -> String {
    let abs = p.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if matches!(out.last(), Some(s) if *s != "..") {
                    out.pop();
                } else if !abs {
                    out.push("..");
                }
            }
            s => out.push(s),
        }
    }
    let body = out.join("/");
    let trailing = p.ends_with('/') && !body.is_empty();
    let mut s = if abs {
        format!("/{body}")
    } else if body.is_empty() {
        ".".into()
    } else {
        body
    };
    if trailing {
        s.push('/');
    }
    s
}

fn basename(rel: &str) -> String {
    rel.rsplit('/').next().unwrap_or("").to_string()
}

fn mode_of(meta: &Metadata) -> u32 {
    meta.permissions().mode()
}

impl LocalContext {
    pub fn open(dir: impl AsRef<Path>, options: &LocalContextOptions) -> Result<LocalContext> {
        let dir = dir.as_ref();
        let shown = dir.display().to_string();
        let root = match fs::canonicalize(dir) {
            Ok(r) => r,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(SitefileError::plain(
                    E_NOT_FOUND,
                    format!("build context '{shown}' does not exist"),
                ))
            }
            Err(e) => {
                return Err(SitefileError::plain(
                    E_NOT_FOUND,
                    format!("cannot resolve build context '{shown}': {e}"),
                ))
            }
        };
        let st = fs::metadata(&root).map_err(|e| {
            SitefileError::plain(
                E_NOT_FOUND,
                format!("cannot resolve build context '{shown}': {e}"),
            )
        })?;
        if !st.is_dir() {
            return Err(SitefileError::plain(
                E_UNSUPPORTED_FILE,
                format!("build context '{shown}' is not a directory"),
            ));
        }
        let mut rules = IgnoreRules::empty();
        if options.sitefileignore && fs::symlink_metadata(root.join(SITEFILEIGNORE)).is_ok() {
            // Read through the sandbox: a symlinked .sitefileignore must stay inside the context.
            let probe = LocalContext {
                root: root.clone(),
                ignore: vec![],
                rules: IgnoreRules::empty(),
                allow_hardlinks: true,
            };
            // A broken ignore file fails the build: skipping it could publish files it was meant to exclude.
            let (real, st) = probe.resolve(SITEFILEIGNORE, None, None).map_err(|e| {
                if e.code != E_NOT_FOUND {
                    return e;
                }
                SitefileError::plain(
                    E_NOT_FOUND,
                    format!("'{SITEFILEIGNORE}' is a symlink to a missing file; fix or remove it (its rules would otherwise be skipped)"),
                )
            })?;
            if !st.is_file() {
                return Err(SitefileError::plain(
                    E_UNSUPPORTED_FILE,
                    format!("'{SITEFILEIGNORE}' is not a regular file"),
                ));
            }
            let bytes = fs::read(&real).map_err(|e| {
                SitefileError::plain(
                    E_NOT_FOUND,
                    format!("cannot read '{}': {e}", display(SITEFILEIGNORE)),
                )
            })?;
            rules = IgnoreRules::parse(&nfc(&String::from_utf8_lossy(&bytes)));
        }
        let ignore_list = options
            .ignore
            .clone()
            .unwrap_or_else(|| vec![".git".to_string()]);
        let ignore = ignore_list
            .iter()
            .map(|p| {
                let n = posix_normalize(&nfc(p));
                let n = n.trim_start_matches("./");
                let n = n.trim_start_matches('/');
                n.trim_end_matches('/').to_string()
            })
            .filter(|p| !p.is_empty() && p != ".")
            .collect();
        Ok(LocalContext {
            root,
            ignore,
            rules,
            allow_hardlinks: options.allow_hardlinks,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn inside(&self, real: &Path) -> bool {
        real.starts_with(&self.root)
    }

    fn is_ignored(&self, rel: &str) -> bool {
        self.ignore.iter().any(|ig| {
            rel == ig
                || (rel.len() > ig.len()
                    && rel.starts_with(ig.as_str())
                    && rel.as_bytes()[ig.len()] == b'/')
        })
    }

    /// Hard-linked files may alias files outside the context, which realpath cannot detect.
    fn check_hardlink(&self, st: &Meta, rel: &str, span: Option<&SourceSpan>) -> Result<()> {
        if self.allow_hardlinks || !st.is_file() || st.nlink() <= 1 {
            return Ok(());
        }
        Err(SitefileError::new(
            E_HARDLINK,
            format!(
                "'{}' has {} hard links and may alias a file outside the build context (--allow-hardlinks permits this)",
                display(rel),
                st.nlink()
            ),
            span.cloned(),
        ))
    }

    /// Context-relative POSIX path of a real path inside root.
    fn rel_of_real(&self, real: &Path) -> String {
        match real.strip_prefix(&self.root) {
            Ok(r) => r
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/"),
            Err(_) => String::new(),
        }
    }

    /// The host path of the context-relative (NFC) path `rel`. A non-ASCII
    /// segment is matched by NFC form against its directory listing, so a file
    /// is found whichever normalization its on-disk name uses (macOS stores
    /// what it was given, often NFD; Linux compares bytes). Two names in one
    /// directory with the same NFC form are E_PATH_COLLISION. A segment with
    /// no match is joined as is, so the caller sees the usual not-found error.
    fn host_path(&self, rel: &str, span: Option<&SourceSpan>) -> Result<PathBuf> {
        if rel.is_empty() {
            return Ok(self.root.clone());
        }
        if rel.is_ascii() {
            return Ok(self.root.join(rel));
        }
        let mut cur = self.root.clone();
        let mut done = String::new();
        for seg in rel.split('/') {
            if !done.is_empty() {
                done.push('/');
            }
            done.push_str(seg);
            if seg.is_ascii() {
                cur.push(seg);
                continue;
            }
            let mut matches: Vec<std::ffi::OsString> = Vec::new();
            if let Ok(rd) = fs::read_dir(&cur) {
                for d in rd.flatten() {
                    let name = d.file_name();
                    if name.to_str().is_some_and(|n| nfc(n) == seg) {
                        matches.push(name);
                    }
                }
            }
            if matches.len() > 1 {
                return Err(normalization_collision(&done, span));
            }
            match matches.pop() {
                Some(m) => cur.push(m),
                None => cur.push(seg),
            }
        }
        Ok(cur)
    }

    /// Resolve a context-relative path to its real path and stats, enforcing the sandbox.
    fn resolve(
        &self,
        rel: &str,
        span: Option<&SourceSpan>,
        link_name: Option<&str>,
    ) -> Result<(PathBuf, Meta)> {
        let abs = self.host_path(rel, span)?;
        let real = match fs::canonicalize(&abs) {
            Ok(r) => r,
            Err(e) => {
                let detail = if e.kind() == io::ErrorKind::NotFound
                    || e.kind() == io::ErrorKind::NotADirectory
                {
                    return Err(SitefileError::new(
                        E_NOT_FOUND,
                        match link_name {
                            Some(l) => format!("symlink '{l}' is dangling"),
                            None => {
                                format!("'{}' does not exist in the build context", display(rel))
                            }
                        },
                        span.cloned(),
                    ));
                } else if e.raw_os_error() == Some(ELOOP) {
                    return Err(SitefileError::new(
                        E_UNSUPPORTED_FILE,
                        format!("'{}' is a symlink loop", display(rel)),
                        span.cloned(),
                    ));
                } else {
                    format!("cannot resolve '{}': {e}", display(rel))
                };
                return Err(SitefileError::new(E_NOT_FOUND, detail, span.cloned()));
            }
        };
        if !self.inside(&real) {
            return Err(SitefileError::new(
                E_PATH_ESCAPE,
                match link_name {
                    Some(l) => format!("symlink '{l}' points outside the build context"),
                    None => format!("'{}' resolves outside the build context", display(rel)),
                },
                span.cloned(),
            ));
        }
        let st = crate::platform::stat(&real).map_err(|e| {
            SitefileError::new(
                E_NOT_FOUND,
                format!("cannot resolve '{}': {e}", display(rel)),
                span.cloned(),
            )
        })?;
        Ok((real, st))
    }

    fn kind_of(st: &Metadata, rel: &str, span: Option<&SourceSpan>) -> Result<EntryKind> {
        if st.is_file() {
            return Ok(EntryKind::File);
        }
        if st.is_dir() {
            return Ok(EntryKind::Directory);
        }
        Err(SitefileError::new(
            E_UNSUPPORTED_FILE,
            format!("'{}' is not a regular file or directory (sockets, fifos and devices are not supported)", display(rel)),
            span.cloned(),
        ))
    }

    pub fn stat(&self, rel: &str, span: Option<&SourceSpan>) -> Result<EntryKind> {
        let (_, st) = self.resolve(rel, span, None)?;
        Self::kind_of(&st, rel, span)
    }

    /// The real path of a context-relative path, with the sandbox checks of every read (an imported Sitefile's context).
    pub fn real_path(&self, rel: &str, span: Option<&SourceSpan>) -> Result<PathBuf> {
        Ok(self.resolve(rel, span, None)?.0)
    }

    fn read_bytes(real: &Path, rel: &str, span: Option<&SourceSpan>) -> Result<Vec<u8>> {
        fs::read(real).map_err(|e| {
            SitefileError::new(
                E_NOT_FOUND,
                format!("cannot read '{}': {e}", display(rel)),
                span.cloned(),
            )
        })
    }

    /// A context file as a [`Resource::File`].
    pub fn read_file(&self, rel: &str, span: Option<&SourceSpan>) -> Result<Resource> {
        let (real, st) = self.resolve(rel, span, None)?;
        if Self::kind_of(&st, rel, span)? != EntryKind::File {
            return Err(SitefileError::new(
                E_UNSUPPORTED_FILE,
                format!("'{}' is a directory, not a file", display(rel)),
                span.cloned(),
            ));
        }
        self.check_hardlink(&st, rel, span)?;
        let bytes = Self::read_bytes(&real, rel, span)?;
        let digest = sha256(&bytes);
        Ok(Resource::File {
            bytes,
            mode: if mode_of(&st) & 0o111 != 0 {
                0o755
            } else {
                0o644
            },
            basename: basename(rel),
            provenance: Provenance {
                uri: format!("./{rel}"),
                resolved_url: None,
                digest,
                git_commit: None,
            },
        })
    }

    /// A context file's real path and bytes, with the usual sandbox, symlink and hard-link rules.
    pub fn real_file(&self, rel: &str, span: Option<&SourceSpan>) -> Result<(PathBuf, Vec<u8>)> {
        let (real, _) = self.resolve(rel, span, None)?;
        match self.read_file(rel, span)? {
            Resource::File { bytes, .. } => Ok((real, bytes)),
            _ => unreachable!("read_file returns a file resource"),
        }
    }

    /// A context directory as a [`Resource::Tree`]. `.sitefileignore` rules and
    /// the ignore list apply below `rel`, never to `rel` itself.
    pub fn read_tree(
        &self,
        rel: &str,
        origin: &str,
        span: Option<&SourceSpan>,
    ) -> Result<Resource> {
        self.read_tree_excluding(rel, origin, span, None)
    }

    /// [`read_tree`](Self::read_tree) without the paths `exclude` (COPY --exclude) matches, relative
    /// to `rel`, like `.sitefileignore` rules (excluded directories are not read).
    pub fn read_tree_excluding(
        &self,
        rel: &str,
        origin: &str,
        span: Option<&SourceSpan>,
        exclude: Option<&IgnoreRules>,
    ) -> Result<Resource> {
        let (real, st) = self.resolve(rel, span, None)?;
        if Self::kind_of(&st, rel, span)? != EntryKind::Directory {
            return Err(SitefileError::new(
                E_UNSUPPORTED_FILE,
                format!("'{}' is a file, not a directory", display(rel)),
                span.cloned(),
            ));
        }
        let mut files: Vec<(String, Node)> = Vec::new();
        let mut ancestors = HashSet::new();
        ancestors.insert(real.clone());
        self.walk(
            &real,
            rel,
            "",
            origin,
            &mut ancestors,
            &mut files,
            span,
            exclude,
        )?;
        // With the instruction's span: two names that differ only in case (possible on a
        // case-sensitive file system) fail at the line that read the directory.
        let tree = Tree::empty().with_files(files, ConflictPolicy::Error, span)?;
        let digest = tree.digest();
        Ok(Resource::Tree {
            tree,
            provenance: Provenance {
                uri: if rel.is_empty() {
                    "./".into()
                } else {
                    format!("./{rel}")
                },
                resolved_url: None,
                digest,
                git_commit: None,
            },
        })
    }

    /// * `real_dir` — real path of the directory being walked
    /// * `ctx_rel` — logical context-relative path of that directory
    /// * `tree_rel` — path of that directory inside the resulting tree (`""` = root)
    /// * `ancestors` — real paths of directories on the current walk path (cycle detection)
    #[allow(clippy::too_many_arguments)]
    fn walk(
        &self,
        real_dir: &Path,
        ctx_rel: &str,
        tree_rel: &str,
        origin: &str,
        ancestors: &mut HashSet<PathBuf>,
        out: &mut Vec<(String, Node)>,
        span: Option<&SourceSpan>,
        exclude: Option<&IgnoreRules>,
    ) -> Result<()> {
        let rd = fs::read_dir(real_dir).map_err(|e| {
            SitefileError::new(
                E_NOT_FOUND,
                format!("cannot read directory '{}': {e}", display(ctx_rel)),
                span.cloned(),
            )
        })?;
        let mut entries: Vec<(String, bool)> = Vec::new();
        for d in rd {
            let d = d.map_err(|e| {
                SitefileError::new(
                    E_NOT_FOUND,
                    format!("cannot read directory '{}': {e}", display(ctx_rel)),
                    span.cloned(),
                )
            })?;
            let name = d.file_name().into_string().map_err(|n| {
                SitefileError::new(
                    E_UNSUPPORTED_FILE,
                    format!(
                        "'{}' contains a file name that is not valid UTF-8: {}",
                        display(ctx_rel),
                        n.to_string_lossy()
                    ),
                    span.cloned(),
                )
            })?;
            let is_link = d.file_type().map(|t| t.is_symlink()).unwrap_or(false);
            entries.push((name, is_link));
        }
        entries.sort();
        // Tree paths are NFC, so two names that differ only in normalization (possible
        // on Linux, not on macOS) would be one path: refuse rather than pick one.
        let mut seen: HashSet<String> = HashSet::new();
        for (raw, is_link) in entries {
            let name = nfc(&raw).into_owned();
            if !seen.insert(name.clone()) {
                return Err(normalization_collision(
                    &if ctx_rel.is_empty() {
                        name
                    } else {
                        format!("{ctx_rel}/{name}")
                    },
                    span,
                ));
            }
            let child_ctx = if ctx_rel.is_empty() {
                name.clone()
            } else {
                format!("{ctx_rel}/{name}")
            };
            let child_tree = if tree_rel.is_empty() {
                name.clone()
            } else {
                format!("{tree_rel}/{name}")
            };
            if self.is_ignored(&child_ctx) {
                continue;
            }
            let mut child_real = real_dir.join(&raw);
            let st: Meta;
            if is_link {
                let (r, s) = self.resolve(&child_ctx, span, Some(&display(&child_ctx)))?;
                child_real = r;
                st = s;
                // A link to an ignored location (e.g. .git) is skipped too.
                if self.is_ignored(&self.rel_of_real(&child_real)) {
                    continue;
                }
            } else {
                st = crate::platform::lstat(&child_real).map_err(|e| {
                    SitefileError::new(
                        E_NOT_FOUND,
                        format!("cannot resolve '{}': {e}", display(&child_ctx)),
                        span.cloned(),
                    )
                })?;
            }

            if self.rules.ignores(&child_ctx, st.is_dir()) {
                continue;
            }
            if exclude.is_some_and(|x| x.ignores(&child_tree, st.is_dir())) {
                continue;
            }

            if st.is_file() {
                self.check_hardlink(&st, &child_ctx, span)?;
                let bytes = Self::read_bytes(&child_real, &child_ctx, span)?;
                out.push((
                    format!("/{child_tree}"),
                    make_file(bytes, origin, mode_of(&st)),
                ));
            } else if st.is_dir() {
                if ancestors.contains(&child_real) {
                    continue; // directory symlink cycle
                }
                ancestors.insert(child_real.clone());
                let r = self.walk(
                    &child_real,
                    &child_ctx,
                    &child_tree,
                    origin,
                    ancestors,
                    out,
                    span,
                    exclude,
                );
                ancestors.remove(&child_real);
                r?;
            } else {
                return Err(SitefileError::new(
                    E_UNSUPPORTED_FILE,
                    format!("'{}' is not a regular file or directory (sockets, fifos and devices are not supported)", display(&child_ctx)),
                    span.cloned(),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::error::ErrorCode;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A fresh temp directory, removed on drop.
    pub(crate) struct TempDir(pub PathBuf);
    impl TempDir {
        pub(crate) fn new(prefix: &str) -> TempDir {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let p =
                std::env::temp_dir().join(format!("{prefix}{}-{n}-{nanos}", std::process::id()));
            fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A temp dir with `ctx/` (the build context) and `outside/` next to it.
    fn with_temp(f: impl FnOnce(&Path, &Path)) {
        let t = TempDir::new("sitefile-local-");
        let ctx = t.0.join("ctx");
        let outside = t.0.join("outside");
        fs::create_dir(&ctx).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret.txt"), "secret").unwrap();
        f(&ctx, &outside);
    }

    fn code<T: std::fmt::Debug>(r: Result<T>) -> ErrorCode {
        r.unwrap_err().code
    }
    fn open(ctx: &Path) -> LocalContext {
        LocalContext::open(ctx, &LocalContextOptions::default()).unwrap()
    }
    #[cfg(unix)]
    fn chmod(p: &Path, mode: u32) {
        fs::set_permissions(p, fs::Permissions::from_mode(mode)).unwrap();
    }
    fn file_of(r: Resource) -> (Vec<u8>, u32, String, Provenance) {
        match r {
            Resource::File {
                bytes,
                mode,
                basename,
                provenance,
            } => (bytes, mode, basename, provenance),
            other => panic!("expected file, got {}", other.kind()),
        }
    }
    fn tree_of(r: Resource) -> (Tree, Provenance) {
        match r {
            Resource::Tree { tree, provenance } => (tree, provenance),
            other => panic!("expected tree, got {}", other.kind()),
        }
    }
    fn paths(t: &Tree) -> Vec<String> {
        t.entries().map(|(p, _)| p.clone()).collect()
    }
    fn tree_paths(c: &LocalContext) -> Vec<String> {
        paths(&tree_of(c.read_tree("", "test", None).unwrap()).0)
    }

    const NFC: &str = "caf\u{e9}";
    const NFD: &str = "cafe\u{301}";

    /// True when this file system can hold `a` and `b` as two names in one directory.
    fn holds_both(dir: &Path, a: &str, b: &str) -> bool {
        let probe = dir.join("probe");
        fs::create_dir(&probe).unwrap();
        fs::write(probe.join(a), "1").unwrap();
        let ok = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(probe.join(b))
            .is_ok()
            && fs::read_dir(&probe).unwrap().count() == 2;
        fs::remove_dir_all(&probe).unwrap();
        ok
    }

    #[test]
    fn files_are_found_whichever_normalization_disk_and_sitefile_use() {
        // On macOS the file system finds either spelling; on Linux host_path matches NFC forms.
        for (on_disk, named) in [(NFD, NFC), (NFC, NFD), (NFD, NFD)] {
            with_temp(|ctx, _| {
                fs::create_dir(ctx.join(on_disk)).unwrap();
                fs::write(ctx.join(on_disk).join(format!("{on_disk}.txt")), "hi").unwrap();
                let c = open(ctx);
                let rel = crate::paths::normalize_local(&format!("{named}/{named}.txt"), None)
                    .unwrap()
                    .path;
                let (bytes, _, base, _) = file_of(c.read_file(&rel, None).unwrap());
                assert_eq!(bytes, b"hi");
                assert_eq!(base, format!("{NFC}.txt"));
                let (t, _) = tree_of(c.read_tree("", "test", None).unwrap());
                assert_eq!(paths(&t), vec![format!("/{NFC}/{NFC}.txt")]);
            });
        }
    }

    #[test]
    fn names_that_differ_only_in_normalization_collide() {
        with_temp(|ctx, _| {
            if !holds_both(ctx, NFC, NFD) {
                return; // macOS: the file system refuses the second name
            }
            fs::write(ctx.join(NFC), "1").unwrap();
            fs::write(ctx.join(NFD), "2").unwrap();
            let c = open(ctx);
            let e = c.read_tree("", "test", None).unwrap_err();
            assert_eq!(e.code, E_PATH_COLLISION);
            assert!(
                e.detail.contains("differ only in Unicode normalization"),
                "{}",
                e.detail
            );
            assert_eq!(code(c.read_file(NFC, None)), E_PATH_COLLISION);
        });
    }

    #[test]
    fn names_that_differ_only_in_case_collide_in_the_tree() {
        with_temp(|ctx, _| {
            if !holds_both(ctx, "A.txt", "a.txt") {
                return; // case-insensitive file system
            }
            fs::write(ctx.join("A.txt"), "1").unwrap();
            fs::write(ctx.join("a.txt"), "2").unwrap();
            let e = open(ctx).read_tree("", "test", None).unwrap_err();
            assert_eq!(e.code, E_PATH_COLLISION);
            assert!(
                e.detail
                    .starts_with("'/a.txt' differs only in case from '/A.txt'"),
                "{}",
                e.detail
            );
        });
    }

    #[test]
    fn open_resolves_the_real_root() {
        with_temp(|ctx, _| {
            let c = open(ctx);
            assert_eq!(c.root, fs::canonicalize(ctx).unwrap());
            assert_eq!(
                code(LocalContext::open(
                    ctx.join("nope"),
                    &LocalContextOptions::default()
                )),
                E_NOT_FOUND
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn stat_and_read_file() {
        with_temp(|ctx, _| {
            fs::create_dir(ctx.join("src")).unwrap();
            fs::write(ctx.join("src/a.txt"), "A").unwrap();
            fs::write(ctx.join("run.sh"), "#!/bin/sh").unwrap();
            chmod(&ctx.join("run.sh"), 0o700);
            chmod(&ctx.join("src/a.txt"), 0o600);
            let c = open(ctx);
            assert_eq!(c.stat("", None).unwrap(), EntryKind::Directory);
            assert_eq!(c.stat("src", None).unwrap(), EntryKind::Directory);
            assert_eq!(c.stat("src/a.txt", None).unwrap(), EntryKind::File);
            assert_eq!(code(c.stat("missing", None)), E_NOT_FOUND);
            assert_eq!(code(c.stat("src/a.txt/x", None)), E_NOT_FOUND);

            let (bytes, mode, base, prov) = file_of(c.read_file("src/a.txt", None).unwrap());
            assert_eq!(bytes, b"A");
            assert_eq!(mode, 0o644);
            assert_eq!(base, "a.txt");
            assert_eq!(prov.uri, "./src/a.txt");
            assert_eq!(prov.digest, sha256(b"A"));
            assert_eq!(file_of(c.read_file("run.sh", None).unwrap()).1, 0o755);
            assert_eq!(code(c.read_file("src", None)), E_UNSUPPORTED_FILE);
        });
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_escaping_the_context_are_rejected() {
        with_temp(|ctx, outside| {
            symlink(outside.join("secret.txt"), ctx.join("leak.txt")).unwrap();
            symlink(outside, ctx.join("leakdir")).unwrap();
            symlink("../outside/secret.txt", ctx.join("rel-leak")).unwrap();
            let c = open(ctx);
            assert_eq!(code(c.stat("leak.txt", None)), E_PATH_ESCAPE);
            assert_eq!(code(c.read_file("leak.txt", None)), E_PATH_ESCAPE);
            assert_eq!(code(c.read_file("rel-leak", None)), E_PATH_ESCAPE);
            assert_eq!(code(c.stat("leakdir", None)), E_PATH_ESCAPE);
            assert_eq!(code(c.read_file("leakdir/secret.txt", None)), E_PATH_ESCAPE);
            assert_eq!(code(c.read_tree("leakdir", "test", None)), E_PATH_ESCAPE);
        });
    }

    #[cfg(unix)]
    #[test]
    fn read_tree_fails_naming_a_link_that_escapes() {
        with_temp(|ctx, outside| {
            fs::create_dir(ctx.join("site")).unwrap();
            fs::write(ctx.join("site/ok.txt"), "ok").unwrap();
            symlink(outside, ctx.join("site/evil")).unwrap();
            let e = open(ctx).read_tree("site", "test", None).unwrap_err();
            assert_eq!(e.code, E_PATH_ESCAPE);
            assert!(e.message().contains("site/evil"), "{}", e.message());
        });
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_inside_the_context_are_followed() {
        with_temp(|ctx, _| {
            fs::create_dir(ctx.join("real")).unwrap();
            fs::write(ctx.join("real/x.txt"), "X").unwrap();
            fs::create_dir(ctx.join("site")).unwrap();
            symlink("../real/x.txt", ctx.join("site/x-link.txt")).unwrap();
            symlink("../real", ctx.join("site/dir-link")).unwrap();
            let c = open(ctx);
            let (bytes, _, base, _) = file_of(c.read_file("site/x-link.txt", None).unwrap());
            assert_eq!(bytes, b"X");
            assert_eq!(base, "x-link.txt");
            let (t, _) = tree_of(c.read_tree("site", "COPY", None).unwrap());
            assert_eq!(paths(&t), vec!["/dir-link/x.txt", "/x-link.txt"]);
        });
    }

    #[cfg(unix)]
    #[test]
    fn directory_symlink_cycles_are_skipped() {
        with_temp(|ctx, _| {
            fs::create_dir_all(ctx.join("a/b")).unwrap();
            fs::write(ctx.join("a/b/f.txt"), "f").unwrap();
            symlink("..", ctx.join("a/b/up")).unwrap();
            symlink(".", ctx.join("a/self")).unwrap();
            let (t, _) = tree_of(open(ctx).read_tree("a", "COPY", None).unwrap());
            assert_eq!(paths(&t), vec!["/b/f.txt"]);
        });
    }

    #[cfg(unix)]
    #[test]
    fn symlink_loops_are_unsupported() {
        with_temp(|ctx, _| {
            symlink("loop-b", ctx.join("loop-a")).unwrap();
            symlink("loop-a", ctx.join("loop-b")).unwrap();
            assert_eq!(code(open(ctx).stat("loop-a", None)), E_UNSUPPORTED_FILE);
        });
    }

    #[cfg(unix)]
    #[test]
    fn ignore_list_is_honoured_while_walking() {
        with_temp(|ctx, _| {
            fs::create_dir(ctx.join(".git")).unwrap();
            fs::write(ctx.join(".git/HEAD"), "ref").unwrap();
            fs::create_dir_all(ctx.join("dist/x")).unwrap();
            fs::write(ctx.join("dist/x/out.html"), "o").unwrap();
            fs::create_dir_all(ctx.join("sub/.git")).unwrap();
            fs::write(ctx.join("sub/.git/keep"), "k").unwrap();
            fs::write(ctx.join("index.html"), "i").unwrap();
            symlink(".git", ctx.join("gitlink")).unwrap();

            let def = open(ctx);
            assert_eq!(
                tree_paths(&def),
                vec!["/dist/x/out.html", "/index.html", "/sub/.git/keep"]
            );

            let opts = LocalContextOptions {
                ignore: Some(vec![".git".into(), "dist/x".into()]),
                ..Default::default()
            };
            let c = LocalContext::open(ctx, &opts).unwrap();
            let (t, prov) = tree_of(c.read_tree("", "COPY", None).unwrap());
            assert_eq!(paths(&t), vec!["/index.html", "/sub/.git/keep"]);
            assert_eq!(prov.digest, t.digest());
            assert_eq!(prov.uri, "./");
            // The walk under dist skips the ignored subdir too.
            let (sub, _) = tree_of(c.read_tree("dist", "COPY", None).unwrap());
            assert_eq!(sub.len(), 0);
        });
    }

    #[test]
    fn ignore_list_entries_are_normalized() {
        with_temp(|ctx, _| {
            fs::create_dir_all(ctx.join("dist")).unwrap();
            fs::write(ctx.join("dist/a"), "a").unwrap();
            fs::write(ctx.join("b"), "b").unwrap();
            let opts = LocalContextOptions {
                ignore: Some(vec!["./dist/".into(), ".".into()]),
                ..Default::default()
            };
            assert_eq!(
                tree_paths(&LocalContext::open(ctx, &opts).unwrap()),
                vec!["/b"]
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn tree_modes_are_normalized_and_output_is_deterministic() {
        with_temp(|ctx, _| {
            fs::create_dir_all(ctx.join("site/z")).unwrap();
            for n in ["b.txt", "a.txt", "C.txt", "z/y.txt"] {
                fs::write(ctx.join("site").join(n), n).unwrap();
            }
            chmod(&ctx.join("site/a.txt"), 0o711);
            chmod(&ctx.join("site/b.txt"), 0o600);
            let c = open(ctx);
            let (t1, p1) = tree_of(c.read_tree("site", "Sitefile:3 COPY", None).unwrap());
            let (_, p2) = tree_of(c.read_tree("site", "Sitefile:3 COPY", None).unwrap());
            assert_eq!(paths(&t1), vec!["/C.txt", "/a.txt", "/b.txt", "/z/y.txt"]);
            assert_eq!(t1.get("/a.txt").unwrap().mode, 0o755);
            assert_eq!(t1.get("/b.txt").unwrap().mode, 0o644);
            assert_eq!(t1.get("/a.txt").unwrap().origin, "Sitefile:3 COPY");
            assert_eq!(p1.digest, p2.digest);
            assert_eq!(p1.uri, "./site");
            assert_eq!(
                code(c.read_tree("site/a.txt", "x", None)),
                E_UNSUPPORTED_FILE
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn fifos_and_sockets_are_unsupported() {
        with_temp(|ctx, _| {
            fs::create_dir(ctx.join("site")).unwrap();
            let pipe = ctx.join("site/pipe");
            let made = std::process::Command::new("mkfifo")
                .arg(&pipe)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            let _sock = if made {
                None
            } else {
                Some(std::os::unix::net::UnixListener::bind(ctx.join("site/sock")).unwrap())
            };
            let c = open(ctx);
            assert_eq!(code(c.read_tree("site", "COPY", None)), E_UNSUPPORTED_FILE);
            assert_eq!(
                code(c.stat(if made { "site/pipe" } else { "site/sock" }, None)),
                E_UNSUPPORTED_FILE
            );
        });
    }

    // ------------------------------------------------------------ .sitefileignore

    #[test]
    fn sitefileignore_filters_directory_walks() {
        with_temp(|ctx, _| {
            fs::create_dir_all(ctx.join("src/cache")).unwrap();
            fs::create_dir_all(ctx.join("node_modules/x")).unwrap();
            fs::write(
                ctx.join(".sitefileignore"),
                "# c\nnode_modules/\n*.log\n/notes.txt\nsrc/cache/\n",
            )
            .unwrap();
            fs::write(ctx.join("a.html"), "a").unwrap();
            fs::write(ctx.join("a.log"), "l").unwrap();
            fs::write(ctx.join("notes.txt"), "n").unwrap();
            fs::write(ctx.join("src/notes.txt"), "n").unwrap();
            fs::write(ctx.join("src/b.log"), "l").unwrap();
            fs::write(ctx.join("src/cache/c"), "c").unwrap();
            fs::write(ctx.join("node_modules/x/i.js"), "i").unwrap();
            assert_eq!(
                tree_paths(&open(ctx)),
                vec!["/.sitefileignore", "/a.html", "/src/notes.txt"]
            );
        });
    }

    #[test]
    fn sitefileignore_cannot_reinclude_under_excluded_directory() {
        with_temp(|ctx, _| {
            fs::create_dir(ctx.join("build")).unwrap();
            fs::write(ctx.join("build/keep.txt"), "k").unwrap();
            fs::write(ctx.join("ok.txt"), "o").unwrap();
            fs::write(ctx.join(".sitefileignore"), "build/\n!build/keep.txt\n").unwrap();
            assert_eq!(tree_paths(&open(ctx)), vec!["/.sitefileignore", "/ok.txt"]);
        });
    }

    #[test]
    fn sitefileignore_negation_reincludes_within_non_excluded_directory() {
        with_temp(|ctx, _| {
            fs::write(ctx.join("a.md"), "a").unwrap();
            fs::write(ctx.join("keep.md"), "k").unwrap();
            fs::write(ctx.join(".sitefileignore"), "*.md\n!keep.md\n").unwrap();
            assert_eq!(tree_paths(&open(ctx)), vec!["/.sitefileignore", "/keep.md"]);
        });
    }

    #[test]
    fn sitefileignore_never_hides_explicitly_named_file_or_root() {
        with_temp(|ctx, _| {
            fs::create_dir(ctx.join("build")).unwrap();
            fs::write(ctx.join("build/x.txt"), "x").unwrap();
            fs::write(ctx.join("secret.log"), "s").unwrap();
            fs::write(ctx.join(".sitefileignore"), "build/\n*.log\n").unwrap();
            let c = open(ctx);
            assert_eq!(file_of(c.read_file("secret.log", None).unwrap()).0, b"s");
            assert_eq!(
                paths(&tree_of(c.read_tree("build", "test", None).unwrap()).0),
                vec!["/x.txt"]
            );
        });
    }

    #[test]
    fn sitefileignore_can_be_disabled_and_is_optional() {
        with_temp(|ctx, _| {
            fs::write(ctx.join("a.log"), "l").unwrap();
            fs::write(ctx.join(".sitefileignore"), "*.log\n").unwrap();
            let off = LocalContextOptions {
                sitefileignore: false,
                ..Default::default()
            };
            assert_eq!(
                tree_paths(&LocalContext::open(ctx, &off).unwrap()),
                vec!["/.sitefileignore", "/a.log"]
            );
            fs::remove_file(ctx.join(".sitefileignore")).unwrap();
            assert_eq!(tree_paths(&open(ctx)), vec!["/a.log"]);
        });
    }

    #[cfg(unix)]
    #[test]
    fn sitefileignore_symlinked_outside_is_refused() {
        with_temp(|ctx, outside| {
            fs::write(outside.join("rules"), "*.log\n").unwrap();
            symlink(outside.join("rules"), ctx.join(".sitefileignore")).unwrap();
            assert_eq!(
                code(LocalContext::open(ctx, &LocalContextOptions::default())),
                E_PATH_ESCAPE
            );
        });
    }

    #[test]
    fn sitefileignore_that_is_a_directory_is_refused() {
        with_temp(|ctx, _| {
            fs::create_dir(ctx.join(".sitefileignore")).unwrap();
            assert_eq!(
                code(LocalContext::open(ctx, &LocalContextOptions::default())),
                E_UNSUPPORTED_FILE
            );
        });
    }

    #[test]
    fn built_in_ignores_still_apply_with_sitefileignore() {
        with_temp(|ctx, _| {
            fs::create_dir(ctx.join(".git")).unwrap();
            fs::write(ctx.join(".git/HEAD"), "ref").unwrap();
            fs::write(ctx.join(".sitefileignore"), "!.git\n").unwrap();
            assert_eq!(tree_paths(&open(ctx)), vec!["/.sitefileignore"]);
        });
    }

    // ------------------------------------------------------------ hard links

    #[test]
    fn hard_link_to_outside_file_is_refused() {
        with_temp(|ctx, outside| {
            fs::hard_link(outside.join("secret.txt"), ctx.join("innocent.txt")).unwrap();
            let c = open(ctx);
            assert_eq!(code(c.read_file("innocent.txt", None)), E_HARDLINK);
            assert_eq!(code(c.read_tree("", "test", None)), E_HARDLINK);
            #[cfg(unix)]
            {
                symlink("innocent.txt", ctx.join("alias.txt")).unwrap();
                assert_eq!(code(c.read_file("alias.txt", None)), E_HARDLINK);
                assert_eq!(code(c.real_file("alias.txt", None)), E_HARDLINK);
            }
        });
    }

    #[test]
    fn hard_links_are_allowed_with_allow_hardlinks() {
        with_temp(|ctx, outside| {
            fs::hard_link(outside.join("secret.txt"), ctx.join("innocent.txt")).unwrap();
            let c = LocalContext::open(
                ctx,
                &LocalContextOptions {
                    allow_hardlinks: true,
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(
                file_of(c.read_file("innocent.txt", None).unwrap()).0,
                b"secret"
            );
            assert_eq!(tree_paths(&c), vec!["/innocent.txt"]);
        });
    }

    #[test]
    fn hard_linked_files_excluded_by_sitefileignore_are_not_reported() {
        with_temp(|ctx, outside| {
            fs::hard_link(outside.join("secret.txt"), ctx.join("innocent.txt")).unwrap();
            fs::write(ctx.join(".sitefileignore"), "innocent.txt\n").unwrap();
            assert_eq!(tree_paths(&open(ctx)), vec!["/.sitefileignore"]);
        });
    }

    #[test]
    fn regular_files_and_directories_unaffected_by_hard_link_check() {
        with_temp(|ctx, _| {
            fs::create_dir_all(ctx.join("d/e")).unwrap();
            fs::write(ctx.join("d/e/f.txt"), "f").unwrap();
            assert_eq!(tree_paths(&open(ctx)), vec!["/d/e/f.txt"]);
        });
    }

    #[cfg(unix)]
    #[test]
    fn dangling_sitefileignore_symlink_fails_clearly() {
        with_temp(|ctx, _| {
            symlink(ctx.join("missing-ignore"), ctx.join(".sitefileignore")).unwrap();
            let e = LocalContext::open(ctx, &LocalContextOptions::default()).unwrap_err();
            assert_eq!(e.code, E_NOT_FOUND);
            assert!(
                e.message().contains("symlink to a missing file"),
                "{}",
                e.message()
            );
            LocalContext::open(
                ctx,
                &LocalContextOptions {
                    sitefileignore: false,
                    ..Default::default()
                },
            )
            .unwrap();
        });
    }

    #[cfg(unix)]
    #[test]
    fn real_file_returns_real_path_and_bytes() {
        with_temp(|ctx, _| {
            fs::write(ctx.join("a.txt"), "A").unwrap();
            symlink("a.txt", ctx.join("b.txt")).unwrap();
            let c = open(ctx);
            let (real, bytes) = c.real_file("b.txt", None).unwrap();
            assert_eq!(real, c.root.join("a.txt"));
            assert_eq!(bytes, b"A");
        });
    }

    #[test]
    fn posix_normalize_matches_node() {
        assert_eq!(posix_normalize("./a/../b/"), "b/");
        assert_eq!(posix_normalize("."), ".");
        assert_eq!(posix_normalize("../x"), "../x");
        assert_eq!(posix_normalize("/a/../.."), "/");
    }
}

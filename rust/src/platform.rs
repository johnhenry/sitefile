//! Platform shims (#318): what the rest of the crate needs from `std::os::unix`,
//! with a Windows stand-in for each.
//!
//! On Unix every item here *is* the std item (re-exports and one-line
//! wrappers), so nothing changes there. On Windows:
//!
//! - permission bits are not applied (`OpenOptions::mode`, `DirBuilder::mode`,
//!   [`set_file_mode`] and [`set_mode`] do nothing) and are read back as
//!   `0o666` (`0o444` when read-only), with no execute bits, as Node reports;
//! - file identity ([`Meta`]: volume serial number, file index, link count) comes
//!   from `GetFileInformationByHandle`, which std calls too but does not expose
//!   on stable Rust; `ctime` is the creation time (Windows has no inode change
//!   time in that call).
//!
//! docs/RUST.md, "Windows", lists what this means for users.

#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::io;
#[cfg(unix)]
use std::path::Path;

#[cfg(unix)]
pub use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};

/// File metadata that also carries identity (device, inode, link count).
/// On Unix this is [`fs::Metadata`] itself.
#[cfg(unix)]
pub type Meta = fs::Metadata;

/// `lstat`: metadata of `p` itself (a symlink is not followed).
#[cfg(unix)]
#[inline]
pub fn lstat(p: &Path) -> io::Result<Meta> {
    fs::symlink_metadata(p)
}

/// The user's home directory as Node's `os.homedir()` reads it: `$HOME`
/// (Windows: `%USERPROFILE%`, then `$HOME`). Only Windows callers use it; Unix
/// code reads `HOME` directly, as before.
#[cfg(windows)]
pub fn home_var() -> Option<String> {
    std::env::var("USERPROFILE")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var("HOME").ok())
}

/// `realpath`.
#[cfg(unix)]
#[inline]
pub fn canonicalize<P: AsRef<Path>>(p: P) -> io::Result<std::path::PathBuf> {
    fs::canonicalize(p)
}

/// `stat`: metadata of `p`, following symlinks.
#[cfg(unix)]
#[inline]
pub fn stat(p: &Path) -> io::Result<Meta> {
    fs::metadata(p)
}

/// `fstat`: metadata of an open file.
#[cfg(unix)]
#[inline]
pub fn fstat(f: &fs::File) -> io::Result<Meta> {
    f.metadata()
}

/// `fchmod`.
#[cfg(unix)]
#[inline]
pub fn set_file_mode(f: &fs::File, mode: u32) -> io::Result<()> {
    f.set_permissions(fs::Permissions::from_mode(mode))
}

/// `chmod`.
#[cfg(unix)]
#[inline]
pub fn set_mode(p: &Path, mode: u32) -> io::Result<()> {
    fs::set_permissions(p, fs::Permissions::from_mode(mode))
}

/// `pread` of exactly `buf.len()` bytes at `offset`.
#[cfg(unix)]
#[inline]
pub fn read_exact_at(f: &fs::File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(f, buf, offset)
}

#[cfg(windows)]
pub use self::windows::*;

#[cfg(windows)]
mod windows {
    use std::ffi::c_void;
    use std::fs;
    use std::io;
    use std::os::windows::fs::{FileExt, OpenOptionsExt as _};
    use std::os::windows::io::AsRawHandle;
    use std::path::{Path, PathBuf};
    use std::time::UNIX_EPOCH;

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    /// 100 ns intervals between 1601-01-01 and 1970-01-01.
    const EPOCH_DIFF: i64 = 116_444_736_000_000_000;

    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    /// `BY_HANDLE_FILE_INFORMATION`.
    #[repr(C)]
    #[derive(Default)]
    struct ByHandleFileInformation {
        file_attributes: u32,
        creation_time: FileTime,
        last_access_time: FileTime,
        last_write_time: FileTime,
        volume_serial_number: u32,
        file_size_high: u32,
        file_size_low: u32,
        number_of_links: u32,
        file_index_high: u32,
        file_index_low: u32,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetFileInformationByHandle(file: *mut c_void, info: *mut ByHandleFileInformation)
            -> i32;
    }

    /// [`fs::Metadata`] plus the identity fields Unix gets from `stat`.
    #[derive(Debug, Clone)]
    pub struct Meta {
        md: fs::Metadata,
        dev: u64,
        ino: u64,
        nlink: u64,
        /// Creation time as (seconds, nanoseconds) since the Unix epoch.
        created: (i64, i64),
    }

    impl std::ops::Deref for Meta {
        type Target = fs::Metadata;
        fn deref(&self) -> &fs::Metadata {
            &self.md
        }
    }

    fn unix_time(t: &FileTime) -> (i64, i64) {
        let ticks = ((t.high as i64) << 32 | t.low as i64) - EPOCH_DIFF;
        (
            ticks.div_euclid(10_000_000),
            ticks.rem_euclid(10_000_000) * 100,
        )
    }

    fn from_handle(f: &fs::File) -> io::Result<Meta> {
        let md = f.metadata()?;
        let mut info = ByHandleFileInformation::default();
        // SAFETY: the handle is open for the duration of the call and `info` is a
        // properly sized, writable BY_HANDLE_FILE_INFORMATION.
        if unsafe { GetFileInformationByHandle(f.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Meta {
            md,
            dev: info.volume_serial_number as u64,
            ino: (info.file_index_high as u64) << 32 | info.file_index_low as u64,
            nlink: info.number_of_links as u64,
            created: unix_time(&info.creation_time),
        })
    }

    /// `lstat`: opens `p` itself (not a symlink's target) without read access,
    /// as std's `symlink_metadata` does, and asks for its file information.
    pub fn lstat(p: &Path) -> io::Result<Meta> {
        let f = fs::OpenOptions::new()
            .access_mode(0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(p)?;
        from_handle(&f)
    }

    /// `stat`: like [`lstat`], following symlinks.
    pub fn stat(p: &Path) -> io::Result<Meta> {
        let f = fs::OpenOptions::new()
            .access_mode(0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(p)?;
        from_handle(&f)
    }

    pub fn fstat(f: &fs::File) -> io::Result<Meta> {
        from_handle(f)
    }

    /// `realpath`: `fs::canonicalize`, without the `\\?\` verbatim prefix when the
    /// path is an ordinary drive path that means the same without it (as Node's
    /// `fs.realpathSync` returns it), so it compares with paths built from the
    /// working directory or the arguments. UNC and over-long paths keep it.
    pub fn canonicalize<P: AsRef<Path>>(p: P) -> io::Result<PathBuf> {
        let real = fs::canonicalize(p)?;
        Ok(simplify(&real).unwrap_or(real))
    }

    fn simplify(p: &Path) -> Option<PathBuf> {
        let s = p.to_str()?;
        let rest = s.strip_prefix(r"\\?\")?;
        let b = rest.as_bytes();
        if b.len() < 3 || !b[0].is_ascii_alphabetic() || b[1] != b':' || b[2] != b'\\' {
            return None;
        }
        // Over MAX_PATH, or a component Win32 would rewrite (trailing dot or
        // space, a reserved device name): keep the verbatim form.
        if rest.len() >= 260 {
            return None;
        }
        for c in rest[3..].split('\\').filter(|c| !c.is_empty()) {
            if c.ends_with('.') || c.ends_with(' ') {
                return None;
            }
            let stem = c.split('.').next().unwrap_or("").to_ascii_uppercase();
            let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                || (stem.len() == 4
                    && (stem.starts_with("COM") || stem.starts_with("LPT"))
                    && stem.as_bytes()[3].is_ascii_digit());
            if reserved {
                return None;
            }
        }
        Some(PathBuf::from(rest))
    }

    /// Permission bits are not applied on Windows.
    pub fn set_file_mode(_: &fs::File, _: u32) -> io::Result<()> {
        Ok(())
    }

    /// Permission bits are not applied on Windows.
    pub fn set_mode(_: &Path, _: u32) -> io::Result<()> {
        Ok(())
    }

    pub fn read_exact_at(f: &fs::File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
        while !buf.is_empty() {
            match f.seek_read(buf, offset) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "failed to fill whole buffer",
                    ))
                }
                Ok(n) => {
                    buf = &mut buf[n..];
                    offset += n as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn mode_bits(md: &fs::Metadata) -> u32 {
        let kind = if md.file_type().is_symlink() {
            0o120000
        } else if md.is_dir() {
            0o040000
        } else {
            0o100000
        };
        kind | perm_bits(md.permissions().readonly())
    }

    fn perm_bits(readonly: bool) -> u32 {
        if readonly {
            0o444
        } else {
            0o666
        }
    }

    /// The `std::os::unix::fs::MetadataExt` methods this crate uses.
    pub trait MetadataExt {
        fn dev(&self) -> u64;
        fn ino(&self) -> u64;
        fn nlink(&self) -> u64;
        fn size(&self) -> u64;
        fn mode(&self) -> u32;
        fn mtime(&self) -> i64;
        fn mtime_nsec(&self) -> i64;
        fn ctime(&self) -> i64;
        fn ctime_nsec(&self) -> i64;
    }

    fn mtime_of(md: &fs::Metadata) -> (i64, i64) {
        let ns: i128 = match md.modified() {
            Ok(t) => match t.duration_since(UNIX_EPOCH) {
                Ok(d) => d.as_nanos() as i128,
                Err(e) => -(e.duration().as_nanos() as i128),
            },
            Err(_) => 0,
        };
        (
            ns.div_euclid(1_000_000_000) as i64,
            ns.rem_euclid(1_000_000_000) as i64,
        )
    }

    impl MetadataExt for Meta {
        fn dev(&self) -> u64 {
            self.dev
        }
        fn ino(&self) -> u64 {
            self.ino
        }
        fn nlink(&self) -> u64 {
            self.nlink
        }
        fn size(&self) -> u64 {
            self.md.len()
        }
        fn mode(&self) -> u32 {
            mode_bits(&self.md)
        }
        fn mtime(&self) -> i64 {
            mtime_of(&self.md).0
        }
        fn mtime_nsec(&self) -> i64 {
            mtime_of(&self.md).1
        }
        fn ctime(&self) -> i64 {
            self.created.0
        }
        fn ctime_nsec(&self) -> i64 {
            self.created.1
        }
    }

    /// `std::os::unix::fs::PermissionsExt::mode`: `0o666`, or `0o444` when read-only.
    pub trait PermissionsExt {
        fn mode(&self) -> u32;
    }

    impl PermissionsExt for fs::Permissions {
        fn mode(&self) -> u32 {
            perm_bits(self.readonly())
        }
    }

    /// `std::os::unix::fs::OpenOptionsExt::mode`: not applied on Windows.
    pub trait OpenOptionsExt {
        fn mode(&mut self, mode: u32) -> &mut Self;
    }

    impl OpenOptionsExt for fs::OpenOptions {
        fn mode(&mut self, _: u32) -> &mut Self {
            self
        }
    }

    /// `std::os::unix::fs::DirBuilderExt::mode`: not applied on Windows.
    pub trait DirBuilderExt {
        fn mode(&mut self, mode: u32) -> &mut Self;
    }

    impl DirBuilderExt for fs::DirBuilder {
        fn mode(&mut self, _: u32) -> &mut Self {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn identity_and_link_count() {
        let d = std::env::temp_dir().join(format!("sitefile-platform-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let a = d.join("a.txt");
        let b = d.join("b.txt");
        fs::write(&a, "a").unwrap();
        fs::write(&b, "b").unwrap();
        let (ma, mb) = (lstat(&a).unwrap(), lstat(&b).unwrap());
        assert_eq!(ma.nlink(), 1);
        assert_ne!((ma.dev(), ma.ino()), (mb.dev(), mb.ino()));
        let f = fs::File::open(&a).unwrap();
        let mf = fstat(&f).unwrap();
        assert_eq!((mf.dev(), mf.ino()), (ma.dev(), ma.ino()));
        assert_eq!(mf.size(), 1);
        drop(f);
        fs::hard_link(&a, d.join("c.txt")).unwrap();
        let mc = stat(&d.join("c.txt")).unwrap();
        assert_eq!(mc.nlink(), 2);
        assert_eq!((mc.dev(), mc.ino()), (ma.dev(), ma.ino()));
        assert!(lstat(&d).unwrap().is_dir());
        assert_eq!(lstat(&d).unwrap().mode() & 0o170000, 0o040000);
        assert_eq!(ma.mode() & 0o170000, 0o100000);
        assert!(lstat(&d.join("missing")).unwrap_err().kind() == std::io::ErrorKind::NotFound);
        let mut buf = [0u8; 1];
        read_exact_at(&fs::File::open(&b).unwrap(), &mut buf, 0).unwrap();
        assert_eq!(&buf, b"b");
        let _ = fs::remove_dir_all(&d);
    }
}

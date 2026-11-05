//! Archive extraction for `IMPORT … AS tree` (port of `src/resources/archive.ts`).

use crate::error::{ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::paths::normalize_virtual;
use crate::resources::types::ArchiveLimits;
use crate::resources::wbn_reader::{is_web_bundle, read_web_bundle, web_bundle_to_tree};
use crate::tree::{make_file, Node, Tree};
use flate2::read::{DeflateDecoder, MultiGzDecoder};
use std::collections::HashSet;
use std::io::Read;

/// Below this many output bytes the ratio limit is not enforced (tar block padding compresses extremely well).
const RATIO_FLOOR: u64 = 1024 * 1024;

/// Tar framing per entry, worst case: header + padding + a pax/GNU long-name header and its padding.
const TAR_OVERHEAD_PER_ENTRY: u64 = 4 * 512;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Dir,
}

struct Collector<'a> {
    origin: &'a str,
    limits: &'a ArchiveLimits,
    span: Option<&'a SourceSpan>,
    files: Vec<(String, Node)>,
    file_paths: HashSet<String>,
    dirs: HashSet<String>,
    total: u64,
}

impl Collector<'_> {
    fn add(&mut self, name: &str, kind: Kind, data: &[u8], mode: u32) -> Result<()> {
        let path = archive_path(name, self.span)?;
        if path == "/" {
            return Ok(());
        }
        if self.file_paths.contains(&path) || (kind == Kind::File && self.dirs.contains(&path)) {
            return Err(SitefileError::new(E_CONFLICT, format!("{}: duplicate archive entry '{name}'", self.origin), self.span.cloned()));
        }
        if kind == Kind::Dir {
            self.dirs.insert(path);
            return Ok(());
        }
        if self.files.len() as u64 + 1 > self.limits.max_entries {
            return Err(limit(self.origin, format!("archive has more than {} entries", self.limits.max_entries), self.span));
        }
        self.total = self.total.saturating_add(data.len() as u64);
        if self.total > self.limits.max_total_bytes {
            return Err(limit(self.origin, format!("archive expands beyond {} bytes", self.limits.max_total_bytes), self.span));
        }
        self.files.push((path.clone(), make_file(data.to_vec(), format!("{}:{name}", self.origin), mode)));
        self.file_paths.insert(path);
        Ok(())
    }
}

fn limit(origin: &str, why: String, span: Option<&SourceSpan>) -> SitefileError {
    SitefileError::new(E_ARCHIVE_LIMIT, format!("{origin}: {why}"), span.cloned())
}

fn corrupt(origin: &str, why: impl AsRef<str>, span: Option<&SourceSpan>) -> SitefileError {
    SitefileError::new(E_ARCHIVE, format!("{origin}: {}", why.as_ref()), span.cloned())
}

/// Extract a tar, tar.gz/tgz, zip or Web Bundle (b1/b2) archive into a Tree. The format is detected
/// from magic bytes, never from the name. Nothing is inflated beyond the limits,
/// and unsafe entries (absolute or `..` paths, backslashes, NUL, symlinks, hard
/// links, devices) are rejected, not skipped. A single top-level directory is
/// kept as-is.
pub fn extract_archive(bytes: &[u8], origin: &str, limits: &ArchiveLimits, span: Option<&SourceSpan>) -> Result<Tree> {
    if is_web_bundle(bytes) {
        return web_bundle_to_tree(&read_web_bundle(bytes, origin, limits, span)?, origin, span);
    }
    let mut c = Collector { origin, limits, span, files: Vec::new(), file_paths: HashSet::new(), dirs: HashSet::new(), total: 0 };
    if is_gzip(bytes) {
        let tar = gunzip(bytes, origin, limits, span)?;
        if !is_tar(&tar) {
            return Err(SitefileError::new(E_TYPE, format!("{origin}: gzip content is not a tar archive"), span.cloned()));
        }
        read_tar(&tar, &mut c)?;
    } else if is_zip(bytes) {
        read_zip(bytes, &mut c)?;
    } else if is_tar(bytes) {
        read_tar(bytes, &mut c)?;
    } else {
        return Err(SitefileError::new(
            E_TYPE,
            format!("{origin}: not a recognized archive (expected tar, tar.gz, zip or web bundle)"),
            span.cloned(),
        ));
    }
    Tree::from_files(c.files)
}

fn is_gzip(b: &[u8]) -> bool {
    b.len() > 2 && b[0] == 0x1f && b[1] == 0x8b
}
fn is_zip(b: &[u8]) -> bool {
    b.len() >= 4 && b[0] == 0x50 && b[1] == 0x4b && ((b[2] == 3 && b[3] == 4) || (b[2] == 5 && b[3] == 6))
}
fn is_tar(b: &[u8]) -> bool {
    b.len() >= 512
        && (&b[257..262] == b"ustar"
            || (tar_checksum_ok(b, 0) && b[..512].iter().any(|&x| x != 0))
            || (b.len() == 1024 && b.iter().all(|&x| x == 0))) // an empty tar
}

fn starts_with_drive(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// Validate an entry name and return its normalized virtual path.
fn archive_path(name: &str, span: Option<&SourceSpan>) -> Result<String> {
    let bad = |why: &str| {
        SitefileError::new(E_ARCHIVE_UNSAFE, format!("unsafe archive entry '{}': {why}", name.replace('\0', "\\0")), span.cloned())
    };
    if name.contains('\0') {
        return Err(bad("NUL byte"));
    }
    if name.contains('\\') {
        return Err(bad("backslash in path"));
    }
    if name.starts_with('/') || starts_with_drive(name) {
        return Err(bad("absolute path"));
    }
    if name.split('/').any(|s| s == "..") {
        return Err(bad("'..' segment"));
    }
    Ok(normalize_virtual(&format!("/{name}"), span, "archive entry")?.path)
}

// ------------------------------------------------------------------ gzip

/// Read at most `cap` bytes; `Ok(None)` if the stream has more.
fn read_capped(mut r: impl Read, cap: u64) -> std::io::Result<Option<Vec<u8>>> {
    let mut out = Vec::new();
    let n = r.by_ref().take(cap.saturating_add(1)).read_to_end(&mut out)?;
    Ok(if n as u64 > cap { None } else { Some(out) })
}

fn gunzip(bytes: &[u8], origin: &str, limits: &ArchiveLimits, span: Option<&SourceSpan>) -> Result<Vec<u8>> {
    let by_ratio = (bytes.len() as u64).saturating_mul(limits.max_ratio).max(RATIO_FLOOR);
    // The inflated stream is tar: file data plus headers and padding for every entry.
    let by_total = limits
        .max_total_bytes
        .saturating_add(limits.max_entries.saturating_add(2).saturating_mul(TAR_OVERHEAD_PER_ENTRY));
    match read_capped(MultiGzDecoder::new(bytes), by_ratio.min(by_total)) {
        Ok(Some(out)) => Ok(out),
        Ok(None) => Err(too_large(origin, by_ratio <= by_total, limits, span)),
        Err(_) => Err(corrupt(origin, "corrupt compressed data", span)),
    }
}

fn too_large(origin: &str, ratio: bool, limits: &ArchiveLimits, span: Option<&SourceSpan>) -> SitefileError {
    if ratio {
        limit(origin, format!("compression ratio exceeds {}:1", limits.max_ratio), span)
    } else {
        limit(origin, format!("archive expands beyond {} bytes", limits.max_total_bytes), span)
    }
}

// ------------------------------------------------------------------- tar

/// `parseInt(s.replace(/[\0 ]/g, ''), radix)`: leading digits, None if there are none.
fn parse_int_prefix(field: &[u8], radix: u32) -> Option<u64> {
    let s: Vec<u8> = field.iter().copied().filter(|&c| c != 0 && c != b' ').collect();
    let mut n: u64 = 0;
    let mut any = false;
    for &c in &s {
        match (c as char).to_digit(radix) {
            Some(d) => {
                n = n.saturating_mul(radix as u64).saturating_add(d as u64);
                any = true;
            }
            None => break,
        }
    }
    any.then_some(n)
}

fn tar_checksum_ok(b: &[u8], off: usize) -> bool {
    if off + 512 > b.len() {
        return false;
    }
    let mut sum: u64 = 0;
    for i in 0..512 {
        sum += if (148..156).contains(&i) { 32 } else { b[off + i] as u64 };
    }
    parse_int_prefix(&b[off + 148..off + 156], 8) == Some(sum)
}

/// A numeric header field: octal text, or GNU base-256. None if unparsable.
fn tar_number(b: &[u8], from: usize, to: usize) -> Option<u64> {
    if b[from] & 0x80 != 0 {
        let mut n = (b[from] & 0x7f) as u64; // GNU base-256
        for &x in &b[from + 1..to] {
            n = n.checked_mul(256)?.checked_add(x as u64)?;
        }
        return Some(n);
    }
    let s: Vec<u8> = b[from..to].iter().copied().filter(|&c| c != 0 && c != b' ').collect();
    if s.is_empty() {
        return Some(0);
    }
    parse_int_prefix(&s, 8)
}

fn c_string(b: &[u8]) -> String {
    let end = b.iter().position(|&x| x == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

/// Supports ustar prefixes, GNU `L` long names and pax `path`/`size` records.
fn read_tar(b: &[u8], c: &mut Collector) -> Result<()> {
    let (origin, span, limits) = (c.origin, c.span, *c.limits);
    let mut off = 0usize;
    let mut next_name: Option<String> = None;
    let mut next_size: Option<u64> = None;
    let mut entries: u64 = 0;
    while off + 512 <= b.len() {
        let h = off;
        if b[h..h + 512].iter().all(|&x| x == 0) {
            return Ok(()); // end-of-archive marker
        }
        if !tar_checksum_ok(b, h) {
            return Err(corrupt(origin, "invalid tar header checksum", span));
        }
        let ty = b[h + 156];
        let size = match next_size.or_else(|| tar_number(b, h + 124, h + 136)) {
            Some(s) if s <= limits.max_total_bytes && s < (1u64 << 53) => s as usize,
            _ => return Err(limit(origin, format!("archive entry larger than {} bytes", limits.max_total_bytes), span)),
        };
        if h + 512 + size > b.len() {
            return Err(corrupt(origin, "truncated tar archive", span));
        }
        let data = &b[h + 512..h + 512 + size];
        off = h + 512 + size.div_ceil(512) * 512;

        match ty {
            b'L' => {
                next_name = Some(c_string(data));
                continue;
            }
            b'x' => {
                let (p, s) = parse_pax(data, origin, span)?;
                next_name = p;
                next_size = s;
                continue;
            }
            b'g' | b'K' => continue, // global pax header / GNU long link name: unused
            _ => {}
        }
        entries += 1;
        if entries > limits.max_entries {
            return Err(limit(origin, format!("archive has more than {} entries", limits.max_entries), span));
        }
        let name = match next_name.take() {
            Some(n) => n,
            None => {
                let mut name = c_string(&b[h..h + 100]);
                // Only POSIX ustar ('ustar\0') has a name prefix; old GNU ('ustar ') keeps atime/ctime there.
                if &b[h + 257..h + 263] == b"ustar\0" {
                    let prefix = c_string(&b[h + 345..h + 500]);
                    if !prefix.is_empty() {
                        name = format!("{prefix}/{name}");
                    }
                }
                name
            }
        };
        next_size = None;
        match ty {
            b'0' | 0 | b'7' => {
                // parseInt NaN & 0o111 is 0: an unparsable mode is 0o644.
                let mode = tar_number(b, h + 100, h + 108).map(|m| m as u32).unwrap_or(0o644);
                c.add(&name, Kind::File, data, mode)?
            }
            b'5' => c.add(&name, Kind::Dir, data, 0o755)?,
            _ => {
                let what = match ty {
                    b'1' => "hard link",
                    b'2' => "symlink",
                    _ => "special file",
                };
                return Err(SitefileError::new(
                    E_ARCHIVE_UNSAFE,
                    format!("unsafe archive entry '{name}': {what} entries are not allowed"),
                    span.cloned(),
                ));
            }
        }
    }
    Ok(())
}

/// `parseInt(text, 10)`: optional leading whitespace and sign, then digits.
fn parse_decimal_prefix(s: &[u8]) -> Option<i64> {
    let s = String::from_utf8_lossy(s);
    let t = s.trim_start();
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let mut n: i64 = 0;
    let mut any = false;
    for ch in digits.bytes() {
        if !ch.is_ascii_digit() {
            break;
        }
        n = n.saturating_mul(10).saturating_add((ch - b'0') as i64);
        any = true;
    }
    any.then_some(if neg { -n } else { n })
}

fn parse_pax(data: &[u8], origin: &str, span: Option<&SourceSpan>) -> Result<(Option<String>, Option<u64>)> {
    let mut path: Option<String> = None;
    let mut size: Option<u64> = None;
    let mut pos = 0usize;
    while pos < data.len() {
        let sp = data[pos..].iter().position(|&x| x == b' ').map(|i| pos + i);
        let Some(sp) = sp else { break };
        let len = match parse_decimal_prefix(&data[pos..sp]) {
            Some(l) if l > (sp - pos) as i64 => l as usize,
            _ => break,
        };
        if pos.saturating_add(len) > data.len() {
            break;
        }
        let (start, end) = (sp + 1, pos + len - 1); // drop trailing newline
        let kv = if end > start { String::from_utf8_lossy(&data[start..end]).into_owned() } else { String::new() };
        let eq = kv.find('=');
        let eq = match eq {
            Some(e) if e > 0 => e,
            _ => return Err(corrupt(origin, "malformed pax header record", span)),
        };
        let (key, value) = (&kv[..eq], &kv[eq + 1..]);
        if key == "path" {
            path = Some(value.to_string());
        }
        if key == "size" {
            if value.is_empty() || !value.bytes().all(|x| x.is_ascii_digit()) {
                return Err(corrupt(origin, format!("malformed pax size '{value}'"), span));
            }
            size = Some(value.parse::<u64>().unwrap_or(u64::MAX));
        }
        pos += len;
    }
    Ok((path, size))
}

// ------------------------------------------------------------------- zip

struct ZipEntry {
    name: String,
    method: u16,
    csize: u32,
    usize_: u32,
    local: u32,
    mode: u32,
    is_dir: bool,
}

fn u16le(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}
fn u32le(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

fn read_zip(b: &[u8], c: &mut Collector) -> Result<()> {
    let (origin, span, limits) = (c.origin, c.span, *c.limits);
    let bad = |why: String| corrupt(origin, why, span);
    let mut eocd: Option<usize> = None;
    if b.len() >= 22 {
        let hi = b.len() - 22;
        let lo = hi.saturating_sub(0xffff);
        for i in (lo..=hi).rev() {
            if u32le(b, i) == 0x06054b50 {
                eocd = Some(i);
                break;
            }
        }
    }
    let eocd = eocd.ok_or_else(|| bad("zip end-of-central-directory record not found".into()))?;
    let count = u16le(b, eocd + 10);
    let mut p = u32le(b, eocd + 16) as usize;
    if count == 0xffff || p == 0xffff_ffff {
        return Err(bad("zip64 archives are not supported".into()));
    }
    if count as u64 > limits.max_entries {
        return Err(limit(origin, format!("archive has more than {} entries", limits.max_entries), span));
    }

    let mut list: Vec<ZipEntry> = Vec::new();
    let mut declared: u64 = 0;
    for _ in 0..count {
        if p + 46 > b.len() || u32le(b, p) != 0x02014b50 {
            return Err(bad("corrupt zip central directory".into()));
        }
        let name_len = u16le(b, p + 28) as usize;
        let advance = 46 + name_len + u16le(b, p + 30) as usize + u16le(b, p + 32) as usize;
        if p + advance > b.len() {
            return Err(bad("corrupt zip central directory".into()));
        }
        let unix = u16le(b, p + 4) >> 8 == 3;
        let attr = u32le(b, p + 38) >> 16;
        let file_type = attr & 0o170000;
        let name = String::from_utf8_lossy(&b[p + 46..p + 46 + name_len]).into_owned();
        let e = ZipEntry {
            method: u16le(b, p + 10),
            csize: u32le(b, p + 20),
            usize_: u32le(b, p + 24),
            local: u32le(b, p + 42),
            mode: if unix { attr & 0o7777 } else { 0o644 },
            // Some tools mark directories only by mode, without a trailing slash.
            is_dir: name.ends_with('/') || (unix && file_type == 0o040000),
            name,
        };
        if u16le(b, p + 8) & 1 != 0 {
            return Err(bad(format!("encrypted zip entry '{}' is not supported", e.name)));
        }
        if e.csize == 0xffff_ffff || e.usize_ == 0xffff_ffff {
            return Err(bad("zip64 archives are not supported".into()));
        }
        if unix && file_type != 0 && file_type != 0o100000 && file_type != 0o040000 {
            return Err(SitefileError::new(
                E_ARCHIVE_UNSAFE,
                format!("unsafe archive entry '{}': symlink or special file entries are not allowed", e.name),
                span.cloned(),
            ));
        }
        declared += e.usize_ as u64;
        list.push(e);
        p += advance;
    }
    if declared > limits.max_total_bytes {
        return Err(limit(origin, format!("archive expands beyond {} bytes", limits.max_total_bytes), span));
    }
    if declared > (b.len() as u64).saturating_mul(limits.max_ratio).max(RATIO_FLOOR) {
        return Err(limit(origin, format!("compression ratio exceeds {}:1", limits.max_ratio), span));
    }

    for e in &list {
        if e.is_dir {
            c.add(&e.name, Kind::Dir, &[], 0o755)?;
            continue;
        }
        let h = e.local as usize;
        if h + 30 > b.len() || u32le(b, h) != 0x04034b50 {
            return Err(bad(format!("corrupt zip local header for '{}'", e.name)));
        }
        let start = h + 30 + u16le(b, h + 26) as usize + u16le(b, h + 28) as usize;
        if start + e.csize as usize > b.len() {
            return Err(bad(format!("truncated zip entry '{}'", e.name)));
        }
        let raw = &b[start..start + e.csize as usize];
        let data: Vec<u8> = match e.method {
            0 => raw.to_vec(),
            8 => {
                // Headers can lie: never inflate more than the declared size.
                match read_capped(DeflateDecoder::new(raw), (e.usize_ as u64).max(1)) {
                    Ok(Some(d)) => d,
                    Ok(None) => return Err(too_large(origin, false, &limits, span)),
                    Err(_) => return Err(bad("corrupt compressed data".into())),
                }
            }
            m => return Err(bad(format!("zip entry '{}' uses unsupported compression method {m}", e.name))),
        };
        if data.len() as u64 != e.usize_ as u64 {
            return Err(bad(format!("zip entry '{}' size does not match its header", e.name)));
        }
        c.add(&e.name, Kind::File, &data, e.mode)?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::error::ErrorCode;
    use crate::resources::types::DEFAULT_ARCHIVE_LIMITS as L;
    use flate2::write::{DeflateEncoder, GzEncoder};
    use flate2::Compression;
    use std::io::Write;

    // ---------------------------------------------------------------- fixtures

    #[derive(Default, Clone)]
    pub(crate) struct TarEntry {
        pub name: String,
        pub data: Vec<u8>,
        /// '0' file (default), '5' dir, '2' symlink, '1' hard link, 'x' pax, 'L' GNU long name.
        pub ty: Option<u8>,
        pub mode: Option<u32>,
        pub linkname: Option<String>,
    }
    pub(crate) fn te(name: &str, data: &str) -> TarEntry {
        TarEntry { name: name.into(), data: data.as_bytes().to_vec(), ..Default::default() }
    }
    fn tt(name: &str, ty: u8) -> TarEntry {
        TarEntry { name: name.into(), ty: Some(ty), ..Default::default() }
    }

    fn octal(n: u64, width: usize) -> String {
        format!("{:0>w$o}\0", n, w = width - 1)
    }
    fn put(h: &mut [u8], at: usize, s: &[u8], max: usize) {
        let n = s.len().min(max);
        h[at..at + n].copy_from_slice(&s[..n]);
    }
    fn with_checksum(h: &mut [u8]) {
        h[148..156].copy_from_slice(b"        ");
        let sum: u64 = h.iter().map(|&x| x as u64).sum();
        let s = format!("{:0>6o}\0 ", sum);
        h[148..156].copy_from_slice(s.as_bytes());
    }

    pub(crate) fn tar_header(e: &TarEntry, size: u64) -> Vec<u8> {
        let mut h = vec![0u8; 512];
        put(&mut h, 0, e.name.as_bytes(), 100);
        put(&mut h, 100, octal(e.mode.unwrap_or(0o644) as u64, 8).as_bytes(), 8);
        put(&mut h, 108, octal(0, 8).as_bytes(), 8);
        put(&mut h, 116, octal(0, 8).as_bytes(), 8);
        put(&mut h, 124, octal(size, 12).as_bytes(), 12);
        put(&mut h, 136, octal(0, 12).as_bytes(), 12);
        h[156] = e.ty.unwrap_or(b'0');
        if let Some(l) = &e.linkname {
            put(&mut h, 157, l.as_bytes(), 100);
        }
        put(&mut h, 257, b"ustar\x0000", 8);
        with_checksum(&mut h);
        h
    }

    pub(crate) fn tar(entries: &[TarEntry]) -> Vec<u8> {
        let mut out = Vec::new();
        for e in entries {
            out.extend(tar_header(e, e.data.len() as u64));
            out.extend(&e.data);
            out.extend(vec![0u8; (512 - e.data.len() % 512) % 512]);
        }
        out.extend(vec![0u8; 1024]);
        out
    }

    pub(crate) fn gzip(b: &[u8]) -> Vec<u8> {
        let mut e = GzEncoder::new(Vec::new(), Compression::default());
        e.write_all(b).unwrap();
        e.finish().unwrap()
    }
    fn deflate_raw(b: &[u8]) -> Vec<u8> {
        let mut e = DeflateEncoder::new(Vec::new(), Compression::default());
        e.write_all(b).unwrap();
        e.finish().unwrap()
    }

    #[derive(Default, Clone)]
    pub(crate) struct ZipIn {
        pub name: String,
        pub data: Vec<u8>,
        pub deflate: bool,
        /// Unix mode in the external attributes (e.g. 0o120777 for a symlink).
        pub mode: Option<u32>,
        /// Lie about the uncompressed size in the headers.
        pub fake_size: Option<u32>,
    }
    fn ze(name: &str, data: &str) -> ZipIn {
        ZipIn { name: name.into(), data: data.as_bytes().to_vec(), ..Default::default() }
    }

    pub(crate) fn zip(entries: &[ZipIn]) -> Vec<u8> {
        let mut local = Vec::new();
        let mut central = Vec::new();
        let mut offset: u32 = 0;
        for e in entries {
            let body = if e.deflate { deflate_raw(&e.data) } else { e.data.clone() };
            let name = e.name.as_bytes();
            let usize_ = e.fake_size.unwrap_or(e.data.len() as u32);
            let mut l = vec![0u8; 30];
            l[0..4].copy_from_slice(&0x04034b50u32.to_le_bytes());
            l[4..6].copy_from_slice(&20u16.to_le_bytes());
            l[8..10].copy_from_slice(&(if e.deflate { 8u16 } else { 0 }).to_le_bytes());
            l[18..22].copy_from_slice(&(body.len() as u32).to_le_bytes());
            l[22..26].copy_from_slice(&usize_.to_le_bytes());
            l[26..28].copy_from_slice(&(name.len() as u16).to_le_bytes());
            let mut c = vec![0u8; 46];
            c[0..4].copy_from_slice(&0x02014b50u32.to_le_bytes());
            c[4..6].copy_from_slice(&((3u16 << 8) | 20).to_le_bytes());
            c[6..8].copy_from_slice(&20u16.to_le_bytes());
            c[10..12].copy_from_slice(&(if e.deflate { 8u16 } else { 0 }).to_le_bytes());
            c[20..24].copy_from_slice(&(body.len() as u32).to_le_bytes());
            c[24..28].copy_from_slice(&usize_.to_le_bytes());
            c[28..30].copy_from_slice(&(name.len() as u16).to_le_bytes());
            let mode = e.mode.unwrap_or(if e.name.ends_with('/') { 0o040755 } else { 0o100644 });
            c[38..42].copy_from_slice(&(mode << 16).to_le_bytes());
            c[42..46].copy_from_slice(&offset.to_le_bytes());
            offset += (l.len() + name.len() + body.len()) as u32;
            local.extend(l);
            local.extend(name);
            local.extend(body);
            central.extend(c);
            central.extend(name);
        }
        let mut end = vec![0u8; 22];
        end[0..4].copy_from_slice(&0x06054b50u32.to_le_bytes());
        end[8..10].copy_from_slice(&(entries.len() as u16).to_le_bytes());
        end[10..12].copy_from_slice(&(entries.len() as u16).to_le_bytes());
        end[12..16].copy_from_slice(&(central.len() as u32).to_le_bytes());
        end[16..20].copy_from_slice(&offset.to_le_bytes());
        let mut out = local;
        out.extend(central);
        out.extend(end);
        out
    }

    // ---------------------------------------------------------------- helpers

    fn x(b: &[u8]) -> Result<Tree> {
        extract_archive(b, "t", &L, None)
    }
    fn paths(t: &Tree) -> Vec<String> {
        t.entries().map(|(p, _)| p.clone()).collect()
    }
    fn rejects(b: &[u8], code: ErrorCode, needle: &str, limits: ArchiveLimits) {
        let e = extract_archive(b, "test", &limits, None).unwrap_err();
        assert_eq!(e.code, code, "{}", e.message());
        assert!(e.message().contains(needle), "{:?} does not contain {needle:?}", e.message());
    }

    fn sample() -> Vec<TarEntry> {
        vec![
            tt("site/", b'5'),
            te("site/index.html", "<h1>hi</h1>"),
            TarEntry { mode: Some(0o700), ..te("site/bin/run", "#!/bin/sh") },
            TarEntry { mode: Some(0o600), ..te("site/css/a.css", "a{}") },
        ]
    }

    // ---------------------------------------------------------------- tests

    #[test]
    fn tar_tgz_and_zip_extract_to_the_same_tree() {
        let zip_entries = vec![
            ZipIn { name: "site/".into(), mode: Some(0o040755), ..Default::default() },
            ZipIn { deflate: true, ..ze("site/index.html", "<h1>hi</h1>") },
            ZipIn { mode: Some(0o100700), ..ze("site/bin/run", "#!/bin/sh") },
            ZipIn { deflate: true, mode: Some(0o100600), ..ze("site/css/a.css", "a{}") },
        ];
        let trees = [x(&tar(&sample())).unwrap(), x(&gzip(&tar(&sample()))).unwrap(), x(&zip(&zip_entries)).unwrap()];
        for t in &trees {
            assert_eq!(paths(t), vec!["/site/bin/run", "/site/css/a.css", "/site/index.html"]);
            assert_eq!(&*t.get("/site/index.html").unwrap().bytes, b"<h1>hi</h1>");
            assert_eq!(t.get("/site/bin/run").unwrap().mode, 0o755);
            assert_eq!(t.get("/site/css/a.css").unwrap().mode, 0o644);
            assert_eq!(t.get("/site/index.html").unwrap().origin, "t:site/index.html");
        }
        assert_eq!(trees[0].digest(), trees[1].digest());
        assert_eq!(trees[0].digest(), trees[2].digest());
    }

    #[test]
    fn empty_archives_and_dot_slash_prefixes() {
        assert_eq!(x(&zip(&[])).unwrap().len(), 0);
        assert_eq!(x(&tar(&[])).unwrap().len(), 0);
        assert_eq!(paths(&x(&tar(&[te("./a/b.txt", "x")])).unwrap()), vec!["/a/b.txt"]);
    }

    fn pax_record(body: &str) -> String {
        let mut len = body.len() + 1;
        while len.to_string().len() + body.len() != len {
            len = len.to_string().len() + body.len();
        }
        format!("{len}{body}")
    }

    #[test]
    fn pax_and_gnu_long_names_are_supported() {
        let long = format!("deep/{}/file.txt", "d".repeat(150));
        let via_pax = tar(&[TarEntry { ty: Some(b'x'), ..te("PaxHeader", &pax_record(&format!(" path={long}\n"))) }, te("short", "p")]);
        assert_eq!(paths(&x(&via_pax).unwrap()), vec![format!("/{long}")]);
        let via_gnu = tar(&[TarEntry { ty: Some(b'L'), ..te("././@LongLink", &format!("{long}\0")) }, te("short", "g")]);
        assert_eq!(paths(&x(&via_gnu).unwrap()), vec![format!("/{long}")]);
    }

    #[test]
    fn pax_size_overrides_the_header_size() {
        let data = "0123456789";
        let mut b = tar(&[TarEntry { ty: Some(b'x'), ..te("PaxHeader", &pax_record(" size=10\n")) }]);
        b.truncate(b.len() - 1024);
        let mut h = tar_header(&te("f", ""), 0); // header says 0 bytes
        with_checksum(&mut h);
        b.extend(h);
        b.extend(data.as_bytes());
        b.extend(vec![0u8; 502 + 1024]);
        let t = x(&b).unwrap();
        assert_eq!(&*t.get("/f").unwrap().bytes, data.as_bytes());
    }

    #[test]
    fn unknown_format_and_non_tar_gzip_are_e_type() {
        rejects(b"just some text, not an archive", E_TYPE, "not a recognized archive", L);
        rejects(b"", E_TYPE, "not a recognized archive", L);
        rejects(&gzip(b"hello"), E_TYPE, "not a tar archive", L);
    }

    #[test]
    fn unsafe_tar_paths_are_rejected() {
        for name in ["../evil", "a/../../evil", "/etc/passwd", "a\\b", "C:/x", "a/.."] {
            rejects(&tar(&[te(name, "x")]), E_ARCHIVE_UNSAFE, "unsafe archive entry", L);
        }
        // A NUL cannot appear in a plain tar name (it terminates it) but can in a pax path.
        let rec = "path=a\0b\n";
        let len = rec.len() + 3;
        rejects(&tar(&[TarEntry { ty: Some(b'x'), ..te("p", &format!("{len} {rec}")) }, te("x", "x")]), E_ARCHIVE_UNSAFE, "NUL", L);
    }

    #[test]
    fn unsafe_zip_paths_are_rejected() {
        for name in ["../evil", "/abs", "a\\b", "x/../../y", "a\0b"] {
            rejects(&zip(&[ze(name, "x")]), E_ARCHIVE_UNSAFE, "unsafe archive entry", L);
        }
    }

    #[test]
    fn symlink_hard_link_and_device_entries_are_rejected() {
        rejects(&tar(&[TarEntry { linkname: Some("/etc/passwd".into()), ..tt("l", b'2') }]), E_ARCHIVE_UNSAFE, "symlink", L);
        rejects(&tar(&[TarEntry { linkname: Some("x".into()), ..tt("h", b'1') }]), E_ARCHIVE_UNSAFE, "hard link", L);
        rejects(&tar(&[tt("d", b'3')]), E_ARCHIVE_UNSAFE, "special file", L);
        rejects(&zip(&[ZipIn { mode: Some(0o120777), ..ze("l", "/etc/passwd") }]), E_ARCHIVE_UNSAFE, "symlink", L);
    }

    #[test]
    fn too_many_entries() {
        let entries: Vec<TarEntry> = (0..6).map(|i| te(&format!("f{i}"), "x")).collect();
        let zentries: Vec<ZipIn> = (0..6).map(|i| ze(&format!("f{i}"), "x")).collect();
        let five = ArchiveLimits { max_entries: 5, ..L };
        rejects(&tar(&entries), E_ARCHIVE_LIMIT, "more than 5 entries", five);
        rejects(&gzip(&tar(&entries)), E_ARCHIVE_LIMIT, "more than 5 entries", five);
        rejects(&zip(&zentries), E_ARCHIVE_LIMIT, "more than 5 entries", five);
        assert_eq!(extract_archive(&tar(&entries), "t", &ArchiveLimits { max_entries: 6, ..L }, None).unwrap().len(), 6);
    }

    #[test]
    fn oversize_total() {
        let k = ArchiveLimits { max_total_bytes: 1000, ..L };
        let a = "x".repeat(600);
        let b = "y".repeat(600);
        rejects(&tar(&[te("a", &a), te("b", &b)]), E_ARCHIVE_LIMIT, "expands beyond 1000 bytes", k);
        rejects(&zip(&[ze("a", &a), ze("b", &b)]), E_ARCHIVE_LIMIT, "expands beyond 1000 bytes", k);
        rejects(&tar(&[te("a", &"x".repeat(5000))]), E_ARCHIVE_LIMIT, "larger than 1000 bytes", k);
    }

    #[test]
    fn compression_ratio_bombs_stop_early() {
        // 50 MB of zeros gzips to ~50 KB (ratio ~1000:1): rejected at the default 100:1 without inflating it all.
        let zeros = TarEntry { name: "zeros".into(), data: vec![0u8; 50 * 1024 * 1024], ..Default::default() };
        let bomb = gzip(&tar(&[zeros]));
        rejects(&bomb, E_ARCHIVE_LIMIT, "compression ratio exceeds 100:1", L);
        // A low total-size limit also stops it.
        rejects(&bomb, E_ARCHIVE_LIMIT, "expands beyond 1048576 bytes", ArchiveLimits { max_total_bytes: 1024 * 1024, max_ratio: 100_000, ..L });
        // Zip: honest headers over the ratio, and headers that lie about the size.
        let big = vec![0u8; 5 * 1024 * 1024];
        rejects(&zip(&[ZipIn { name: "z".into(), data: big.clone(), deflate: true, ..Default::default() }]), E_ARCHIVE_LIMIT, "ratio", L);
        rejects(&zip(&[ZipIn { name: "z".into(), data: big, deflate: true, fake_size: Some(10), ..Default::default() }]), E_ARCHIVE_LIMIT, "expands", L);
    }

    #[test]
    fn duplicate_paths_are_e_conflict() {
        rejects(&tar(&[te("a", "1"), te("./a", "1")]), E_CONFLICT, "duplicate", L);
        rejects(&zip(&[ze("a", "1"), ze("a", "2")]), E_CONFLICT, "duplicate", L);
        rejects(&tar(&[te("a", "1"), te("a/b", "2")]), E_CONFLICT, "", L);
        rejects(&tar(&[tt("a", b'5'), te("a", "2")]), E_CONFLICT, "duplicate", L);
    }

    #[test]
    fn corrupt_archives_are_e_archive() {
        let mut bad_sum = tar(&[te("a", "x")]);
        bad_sum[0] = 0x7a;
        rejects(&bad_sum, E_ARCHIVE, "checksum", L);
        let mut trunc = tar_header(&te("big", ""), 4096);
        trunc.extend(vec![0u8; 100]);
        rejects(&trunc, E_ARCHIVE, "truncated", L);
        rejects(&zip(&[ze("a", "x")])[..30], E_ARCHIVE, "end-of-central-directory", L);
        let mut gz = gzip(&tar(&[te("a", "x")]));
        let n = gz.len();
        gz[n / 2] ^= 0xff;
        gz[n / 2 + 1] ^= 0xff;
        let e = x(&gz).unwrap_err();
        assert!(matches!(e.code, E_ARCHIVE | E_TYPE), "{}", e.message());
    }

    #[test]
    fn ustar_prefix_is_for_posix_headers_only() {
        let mut posix = tar_header(&te("file.txt", ""), 1);
        put(&mut posix, 345, b"dir\0", 4);
        let mut gnu = tar_header(&te("file.txt", ""), 1);
        put(&mut gnu, 257, b"ustar  \0", 8); // old GNU magic + version
        put(&mut gnu, 345, b"\x01\x02\x03atime", 8); // atime/ctime live here in old GNU headers
        let one = |mut h: Vec<u8>| {
            with_checksum(&mut h);
            h.extend(b"x");
            h.extend(vec![0u8; 511 + 1024]);
            h
        };
        assert_eq!(paths(&x(&one(posix)).unwrap()), vec!["/dir/file.txt"]);
        assert_eq!(paths(&x(&one(gnu)).unwrap()), vec!["/file.txt"]);
    }

    #[test]
    fn gzip_size_cap_leaves_room_for_tar_framing() {
        // 2000 one-byte files: 2000 bytes of data but ~2 MB of tar framing.
        let entries: Vec<TarEntry> = (0..2000).map(|i| te(&format!("f{i}"), "x")).collect();
        let t = extract_archive(&gzip(&tar(&entries)), "t", &ArchiveLimits { max_total_bytes: 4000, max_entries: 2000, max_ratio: 1_000_000 }, None).unwrap();
        assert_eq!(t.len(), 2000);
    }

    #[test]
    fn zip_directories_marked_only_by_mode_are_directories() {
        let z = zip(&[ZipIn { name: "assets".into(), mode: Some(0o040755), ..Default::default() }, ze("assets/app.css", "a{}")]);
        assert_eq!(paths(&x(&z).unwrap()), vec!["/assets/app.css"]);
    }

    #[test]
    fn malformed_pax_records_are_e_archive() {
        let pax = |body: &str| tar(&[TarEntry { ty: Some(b'x'), ..te("PaxHeader", &pax_record(body)) }, te("a", "x")]);
        rejects(&pax(" size=abc\n"), E_ARCHIVE, "malformed pax size 'abc'", L);
        rejects(&pax(" novalue\n"), E_ARCHIVE, "malformed pax header record", L);
    }

    #[test]
    fn gnu_base256_sizes_are_read() {
        let mut h = tar_header(&te("f", ""), 0);
        h[124..136].copy_from_slice(&[0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3]);
        with_checksum(&mut h);
        h.extend(b"abc");
        h.extend(vec![0u8; 509 + 1024]);
        assert_eq!(&*x(&h).unwrap().get("/f").unwrap().bytes, b"abc");
    }
}

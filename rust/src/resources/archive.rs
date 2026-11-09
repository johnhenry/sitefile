//! Archive extraction for `IMPORT … AS tree` (port of `src/resources/archive.ts`).

use crate::error::{ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::paths::normalize_virtual;
use crate::resources::types::ArchiveLimits;
use crate::resources::wbn_reader::{is_web_bundle, read_web_bundle, web_bundle_to_tree};
use crate::tree::{make_file, Node, Tree};
use flate2::{Decompress, FlushDecompress, Status};
use std::borrow::Cow;
use std::collections::HashSet;

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
    /// False: keep only digests (the bytes are read again on demand, [`open_zip_file`]).
    retain: bool,
}

impl Collector<'_> {
    fn add(&mut self, name: &str, kind: Kind, data: &[u8], mode: u32) -> Result<()> {
        let path = archive_path(name, self.span)?;
        if path == "/" {
            return Ok(());
        }
        if self.file_paths.contains(&path) || (kind == Kind::File && self.dirs.contains(&path)) {
            return Err(SitefileError::new(
                E_CONFLICT,
                format!("{}: duplicate archive entry '{name}'", self.origin),
                self.span.cloned(),
            ));
        }
        if kind == Kind::Dir {
            self.dirs.insert(path);
            return Ok(());
        }
        if self.files.len() as u64 + 1 > self.limits.max_entries {
            return Err(limit(
                self.origin,
                format!("archive has more than {} entries", self.limits.max_entries),
                self.span,
            ));
        }
        self.total = self.total.saturating_add(data.len() as u64);
        if self.total > self.limits.max_total_bytes {
            return Err(limit(
                self.origin,
                format!(
                    "archive expands beyond {} bytes",
                    self.limits.max_total_bytes
                ),
                self.span,
            ));
        }
        let node = make_file(data.to_vec(), format!("{}:{name}", self.origin), mode);
        let node = if self.retain {
            node
        } else {
            std::sync::Arc::new(crate::tree::FileNode {
                bytes: std::sync::Arc::from(Vec::<u8>::new()),
                ..(*node).clone()
            })
        };
        self.files.push((path.clone(), node));
        self.file_paths.insert(path);
        Ok(())
    }
}

fn limit(origin: &str, why: String, span: Option<&SourceSpan>) -> SitefileError {
    SitefileError::new(E_ARCHIVE_LIMIT, format!("{origin}: {why}"), span.cloned())
}

fn corrupt(origin: &str, why: impl AsRef<str>, span: Option<&SourceSpan>) -> SitefileError {
    SitefileError::new(
        E_ARCHIVE,
        format!("{origin}: {}", why.as_ref()),
        span.cloned(),
    )
}

/// Extract a tar, tar.gz/tgz, zip or Web Bundle (b1/b2, plain or gzip-wrapped) archive into a Tree. The format is detected
/// from magic bytes, never from the name. Nothing is inflated beyond the limits,
/// and unsafe entries (absolute or `..` paths, backslashes, NUL, symlinks, hard
/// links, devices) are rejected, not skipped. A single top-level directory is
/// kept as-is.
pub fn extract_archive(
    bytes: &[u8],
    origin: &str,
    limits: &ArchiveLimits,
    span: Option<&SourceSpan>,
) -> Result<Tree> {
    if is_web_bundle(bytes) {
        return web_bundle_to_tree(&read_web_bundle(bytes, origin, limits, span)?, origin, span);
    }
    let mut c = Collector {
        origin,
        limits,
        span,
        files: Vec::new(),
        file_paths: HashSet::new(),
        dirs: HashSet::new(),
        total: 0,
        retain: true,
    };
    if is_gzip(bytes) {
        let inner = gunzip(bytes, origin, limits, span)?;
        // gzip(application/webbundle), as @johnhenry/fileable (encode="wbn") and packfile's toArchive() write it
        if is_web_bundle(&inner) {
            return web_bundle_to_tree(
                &read_web_bundle(&inner, origin, limits, span)?,
                origin,
                span,
            );
        }
        if !is_tar(&inner) {
            return Err(SitefileError::new(
                E_TYPE,
                format!("{origin}: gzip content is not a tar archive"),
                span.cloned(),
            ));
        }
        read_tar(&inner, &mut c)?;
    } else if is_zip(bytes) {
        read_zip(bytes, &mut c)?;
    } else if is_tar(bytes) {
        read_tar(bytes, &mut c)?;
    } else {
        let what = other_format(bytes).unwrap_or("expected tar, tar.gz, zip or web bundle");
        return Err(SitefileError::new(
            E_TYPE,
            format!("{origin}: not a recognized archive ({what})"),
            span.cloned(),
        ));
    }
    Tree::from_files(c.files)
}

/// OS metadata that archivers and file managers add next to real files: macOS
/// resource forks (`__MACOSX/`, AppleDouble `._name`), Finder's `.DS_Store` and
/// custom folder `Icon\r`, and Windows' `Thumbs.db` and `desktop.ini`
/// (case-insensitive, as Windows writes them).
pub fn is_os_metadata(path: &str) -> bool {
    if path.split('/').any(|s| s == "__MACOSX") {
        return true;
    }
    let base = path.rsplit('/').next().unwrap_or("");
    base.starts_with("._")
        || base == ".DS_Store"
        || base == "Icon\r"
        || base.eq_ignore_ascii_case("thumbs.db")
        || base.eq_ignore_ascii_case("desktop.ini")
}

/// `tree` without its OS metadata files ([`is_os_metadata`]), and how many were dropped.
pub fn without_os_metadata(tree: Tree) -> Result<(Tree, usize)> {
    let skipped = tree.entries().filter(|(p, _)| is_os_metadata(p)).count();
    if skipped == 0 {
        return Ok((tree, 0));
    }
    let kept: Vec<(String, Node)> = tree
        .entries()
        .filter(|(p, _)| !is_os_metadata(p))
        .map(|(p, n)| (p.clone(), n.clone()))
        .collect();
    Ok((Tree::from_files(kept)?, skipped))
}

/// The warning for [`without_os_metadata`] dropping `skipped` files of `what`.
pub fn os_metadata_warning(what: &str, skipped: usize) -> String {
    format!(
        "{what}: skipped {skipped} OS metadata {} (__MACOSX, ._*, .DS_Store, Thumbs.db, desktop.ini, Icon\\r); --keep-os-metadata keeps them",
        if skipped == 1 { "file" } else { "files" }
    )
}

/// Formats sitefile does not read, by magic bytes: what the file is and what to do instead.
fn other_format(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(&[0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00]) {
        return Some("it is xz-compressed, e.g. .tar.xz; recompress it as .tar.gz or zip");
    }
    if b.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        return Some("it is zstd-compressed, e.g. .tar.zst; recompress it as .tar.gz or zip");
    }
    if b.starts_with(b"BZh") && b.len() > 3 && (b'1'..=b'9').contains(&b[3]) {
        return Some("it is bzip2-compressed, e.g. .tar.bz2; recompress it as .tar.gz or zip");
    }
    if b.starts_with(&[0x37, 0x7a, 0xbc, 0xaf, 0x27, 0x1c]) {
        return Some("it is a 7z archive; convert it to zip or tar");
    }
    if b.starts_with(&[0x52, 0x61, 0x72, 0x21, 0x1a, 0x07]) {
        return Some("it is a RAR archive; convert it to zip or tar");
    }
    None
}

fn is_gzip(b: &[u8]) -> bool {
    b.len() > 2 && b[0] == 0x1f && b[1] == 0x8b
}
fn is_zip(b: &[u8]) -> bool {
    b.len() >= 4
        && b[0] == 0x50
        && b[1] == 0x4b
        && ((b[2] == 3 && b[3] == 4) || (b[2] == 5 && b[3] == 6))
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
        SitefileError::new(
            E_ARCHIVE_UNSAFE,
            format!(
                "unsafe archive entry '{}': {why}",
                name.replace('\0', "\\0")
            ),
            span.cloned(),
        )
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

fn gunzip(
    bytes: &[u8],
    origin: &str,
    limits: &ArchiveLimits,
    span: Option<&SourceSpan>,
) -> Result<Vec<u8>> {
    let by_ratio = (bytes.len() as u64)
        .saturating_mul(limits.max_ratio)
        .max(RATIO_FLOOR);
    // The inflated stream is tar: file data plus headers and padding for every entry.
    let by_total = limits.max_total_bytes.saturating_add(
        limits
            .max_entries
            .saturating_add(2)
            .saturating_mul(TAR_OVERHEAD_PER_ENTRY),
    );
    match inflate_like_node(bytes, true, by_ratio.min(by_total)) {
        Inflated::Done(out) => Ok(out),
        Inflated::TooLarge => Err(too_large(origin, by_ratio <= by_total, limits, span)),
        Inflated::Corrupt => Err(corrupt(origin, "corrupt compressed data", span)),
    }
}

/// Output buffer size of a Node zlib stream (`zlib.constants.Z_DEFAULT_CHUNK`).
const NODE_CHUNK: usize = 16 * 1024;

enum Inflated {
    Done(Vec<u8>),
    TooLarge,
    Corrupt,
}

/// What Node's `zlib.gunzip` (`gzip`) or `zlib.inflateRaw` with
/// `maxOutputLength: cap` makes of `input`, so both implementations report the
/// same error (found by differential fuzzing, #81). Node inflates into 16 KiB
/// buffers and checks the limit each time it hands a buffer on: when one fills,
/// when the input runs out, and at the end. An error (corrupt data, a bad
/// check value) inside a buffer wins over output it never handed on; output
/// before a truncation is handed on first. A gzip member is followed by
/// another while input remains and the next byte is not 0x00 (trailing zero
/// padding is ignored); a raw deflate stream ignores anything after its end.
/// Never inflates more than `cap` + 16 KiB.
fn inflate_like_node(input: &[u8], gzip: bool, cap: u64) -> Inflated {
    let fresh = || {
        if gzip {
            Decompress::new_gzip(15)
        } else {
            Decompress::new(false)
        }
    };
    let mut d = fresh();
    let mut out: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; NODE_CHUNK];
    let mut fill = 0usize;
    let mut pos = 0usize;
    loop {
        let (in0, out0) = (d.total_in(), d.total_out());
        let status = d.decompress(&input[pos..], &mut buf[fill..], FlushDecompress::None);
        let (consumed, produced) = (
            (d.total_in() - in0) as usize,
            (d.total_out() - out0) as usize,
        );
        let Ok(status) = status else {
            return Inflated::Corrupt;
        };
        pos += consumed;
        out.extend_from_slice(&buf[fill..fill + produced]);
        fill += produced;
        if status == Status::StreamEnd {
            if gzip && pos < input.len() && input[pos] != 0 {
                d = fresh(); // the next member, in the same buffer
                continue;
            }
            return if out.len() as u64 > cap {
                Inflated::TooLarge
            } else {
                Inflated::Done(out)
            };
        }
        if fill == NODE_CHUNK {
            if out.len() as u64 > cap {
                return Inflated::TooLarge;
            }
            fill = 0;
        } else if pos == input.len() || (consumed == 0 && produced == 0) {
            // Truncated: what was inflated is handed on, then "unexpected end of file".
            return if out.len() as u64 > cap {
                Inflated::TooLarge
            } else {
                Inflated::Corrupt
            };
        }
    }
}

fn too_large(
    origin: &str,
    ratio: bool,
    limits: &ArchiveLimits,
    span: Option<&SourceSpan>,
) -> SitefileError {
    if ratio {
        limit(
            origin,
            format!("compression ratio exceeds {}:1", limits.max_ratio),
            span,
        )
    } else {
        limit(
            origin,
            format!("archive expands beyond {} bytes", limits.max_total_bytes),
            span,
        )
    }
}

// ------------------------------------------------------------------- tar

/// `parseInt(field.replace(/[\0 ]/g, ''), radix)` on a Latin-1 header field.
fn js_parse_int(field: &[u8], radix: u32) -> f64 {
    let s: Vec<u8> = field
        .iter()
        .copied()
        .filter(|&c| c != 0 && c != b' ')
        .collect();
    js_parse_int_raw(&s, radix)
}

/// JavaScript `parseInt(latin1(s), radix)`: leading white space (of which
/// Latin-1 has tab to CR, space and NBSP), an optional sign, then the longest
/// run of digits. NaN if there are none.
fn js_parse_int_raw(s: &[u8], radix: u32) -> f64 {
    let mut i = s
        .iter()
        .take_while(|&&c| matches!(c, 0x09..=0x0d | b' ' | 0xa0))
        .count();
    let neg = match s.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let mut n = 0f64;
    let mut any = false;
    for &c in &s[i..] {
        match (c as char).to_digit(radix) {
            Some(d) => {
                n = n * radix as f64 + d as f64;
                any = true;
            }
            None => break,
        }
    }
    if !any {
        f64::NAN
    } else if neg {
        -n
    } else {
        n
    }
}

/// `Number.isSafeInteger`.
fn is_safe_integer(n: f64) -> bool {
    n.is_finite() && n.trunc() == n && n.abs() <= 9_007_199_254_740_991.0
}

/// JavaScript ToInt32 (what `n & mask` applies to a number).
fn to_int32(n: f64) -> i32 {
    if !n.is_finite() {
        return 0;
    }
    n.trunc().rem_euclid(4_294_967_296.0) as u32 as i32
}

fn tar_checksum_ok(b: &[u8], off: usize) -> bool {
    if off + 512 > b.len() {
        return false;
    }
    let mut sum: u64 = 0;
    for i in 0..512 {
        sum += if (148..156).contains(&i) {
            32
        } else {
            b[off + i] as u64
        };
    }
    js_parse_int(&b[off + 148..off + 156], 8) == sum as f64
}

/// A numeric header field as TypeScript's `tarNumber` reads it: GNU base-256,
/// or octal text (an empty field is 0). NaN if unparsable; may be negative or
/// (base-256) rounded beyond 2^53, exactly like the JavaScript number.
fn tar_number(b: &[u8], from: usize, to: usize) -> f64 {
    if b[from] & 0x80 != 0 {
        let mut n = (b[from] & 0x7f) as f64; // GNU base-256
        for &x in &b[from + 1..to] {
            n = n * 256.0 + x as f64;
        }
        return n;
    }
    if b[from..to].iter().all(|&c| c == 0 || c == b' ') {
        return 0.0;
    }
    js_parse_int(&b[from..to], 8)
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
        let size = next_size
            .map(|s| s as f64)
            .unwrap_or_else(|| tar_number(b, h + 124, h + 136));
        if !is_safe_integer(size) || size < 0.0 || size > limits.max_total_bytes as f64 {
            return Err(limit(
                origin,
                format!("archive entry larger than {} bytes", limits.max_total_bytes),
                span,
            ));
        }
        let size = size as usize; // -0 is 0
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
            return Err(limit(
                origin,
                format!("archive has more than {} entries", limits.max_entries),
                span,
            ));
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
                // makeFile's `mode & 0o111` on the JavaScript number (NaN is 0: 0o644).
                let mode = if to_int32(tar_number(b, h + 100, h + 108)) & 0o111 != 0 {
                    0o755
                } else {
                    0o644
                };
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

fn parse_pax(
    data: &[u8],
    origin: &str,
    span: Option<&SourceSpan>,
) -> Result<(Option<String>, Option<u64>)> {
    let mut path: Option<String> = None;
    let mut size: Option<u64> = None;
    let mut pos = 0usize;
    while pos < data.len() {
        let sp = data[pos..].iter().position(|&x| x == b' ').map(|i| pos + i);
        let Some(sp) = sp else { break };
        // parseInt(latin1, 10); NaN fails the comparison too.
        let len = js_parse_int_raw(&data[pos..sp], 10);
        if !(len > (sp - pos) as f64) || pos as f64 + len > data.len() as f64 {
            break;
        }
        let len = len as usize;
        let (start, end) = (sp + 1, pos + len - 1); // drop trailing newline
        let kv = if end > start {
            String::from_utf8_lossy(&data[start..end]).into_owned()
        } else {
            String::new()
        };
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
                return Err(corrupt(
                    origin,
                    format!("malformed pax size '{value}'"),
                    span,
                ));
            }
            size = Some(value.parse::<u64>().unwrap_or(u64::MAX));
        }
        pos += len;
    }
    Ok((path, size))
}

// ------------------------------------------------------------------- zip

#[derive(Clone)]
struct ZipEntry {
    name: String,
    /// The name's bytes in the central directory.
    raw_name: Vec<u8>,
    method: u16,
    crc: u32,
    csize: u64,
    usize_: u64,
    local: u64,
    mode: u32,
    is_dir: bool,
}

fn u16le(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}
fn u32le(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

/// IBM437, bytes 0x80–0xFF (0x00–0x7F are ASCII).
const CP437_HIGH: [char; 128] = [
    'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', //
    'É', 'æ', 'Æ', 'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ', //
    'á', 'í', 'ó', 'ú', 'ñ', 'Ñ', 'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»', //
    '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕', '╣', '║', '╗', '╝', '╜', '╛', '┐', //
    '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦', '╠', '═', '╬', '╧', //
    '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐', '▀', //
    'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩', //
    '≡', '±', '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■', '\u{a0}',
];

/// A zip entry name (APPNOTE 4.4.4, 4.6.9, appendix D). With general-purpose
/// bit 11 it is UTF-8. Without it: the Info-ZIP Unicode Path field (0x7075,
/// version 1) when its CRC-32 matches the raw name; else the raw name as UTF-8
/// when it is valid UTF-8 (macOS, 7-Zip and many tools write UTF-8 without the
/// flag; CP437 text of non-ASCII letters is practically never valid UTF-8);
/// else IBM437.
fn zip_name(raw: &[u8], flags: u16, extra: &[u8]) -> String {
    if flags & 0x800 != 0 {
        return String::from_utf8_lossy(raw).into_owned();
    }
    let mut i = 0usize;
    while i + 4 <= extra.len() {
        let id = u16le(extra, i);
        let size = u16le(extra, i + 2) as usize;
        if i + 4 + size > extra.len() {
            break;
        }
        if id == 0x7075 && size >= 5 && extra[i + 4] == 1 {
            let mut crc = flate2::Crc::new();
            crc.update(raw);
            if u32le(extra, i + 5) == crc.sum() {
                if let Ok(name) = std::str::from_utf8(&extra[i + 9..i + 4 + size]) {
                    return name.to_string();
                }
            }
        }
        i += 4 + size;
    }
    if let Ok(name) = std::str::from_utf8(raw) {
        return name.to_string();
    }
    raw.iter()
        .map(|&c| {
            if c < 0x80 {
                c as char
            } else {
                CP437_HIGH[(c - 0x80) as usize]
            }
        })
        .collect()
}

/// The data of the first extra field `id` in `extra`.
fn extra_field(extra: &[u8], id: u16) -> Option<&[u8]> {
    let mut i = 0usize;
    while i + 4 <= extra.len() {
        let size = u16le(extra, i + 2) as usize;
        if i + 4 + size > extra.len() {
            return None;
        }
        if u16le(extra, i) == id {
            return Some(&extra[i + 4..i + 4 + size]);
        }
        i += 4 + size;
    }
    None
}

fn u64le(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
}

/// Random access to a zip's bytes: in memory, or a file read on demand ([`open_zip_file`]).
pub(crate) trait ZipSource {
    fn size(&self) -> usize;
    /// `len` bytes at `at`; the caller has checked that they are inside the source.
    fn read(&self, at: usize, len: usize) -> Result<Cow<'_, [u8]>>;
    fn u16_at(&self, at: usize) -> Result<u16> {
        Ok(u16le(&self.read(at, 2)?, 0))
    }
    fn u32_at(&self, at: usize) -> Result<u32> {
        Ok(u32le(&self.read(at, 4)?, 0))
    }
    fn u64_at(&self, at: usize) -> Result<u64> {
        Ok(u64le(&self.read(at, 8)?, 0))
    }
}

struct MemorySource<'a>(&'a [u8]);

impl ZipSource for MemorySource<'_> {
    fn size(&self) -> usize {
        self.0.len()
    }
    fn read(&self, at: usize, len: usize) -> Result<Cow<'_, [u8]>> {
        Ok(Cow::Borrowed(&self.0[at..at + len]))
    }
}

/// Where one zip entry's data is, after [`zip_index`] checked its headers.
#[derive(Clone)]
struct ZipSpan {
    e: ZipEntry,
    start: usize,
}

/// Read and check a zip's central directory and local headers (no entry data):
/// sizes and counts against the limits, ZIP64 records, names, unsafe entry
/// types, local headers that disagree with the central directory, overlapping
/// entries. In central-directory order.
fn zip_index(
    src: &dyn ZipSource,
    origin: &str,
    limits: &ArchiveLimits,
    span: Option<&SourceSpan>,
) -> Result<Vec<ZipSpan>> {
    let bad = |why: String| corrupt(origin, why, span);
    let len = src.size();
    let mut eocd: Option<usize> = None;
    if len >= 22 {
        // The end record is in the last 22 + 65535 bytes (its comment is at most 65535 bytes).
        let from = (len - 22).saturating_sub(0xffff);
        let tail = src.read(from, len - from)?;
        for i in (0..=tail.len() - 22).rev() {
            if u32le(&tail, i) == 0x06054b50 {
                eocd = Some(from + i);
                break;
            }
        }
    }
    let eocd = eocd.ok_or_else(|| bad("zip end-of-central-directory record not found".into()))?;
    let mut count = src.u16_at(eocd + 10)? as u64;
    let mut p0 = src.u32_at(eocd + 16)? as u64;
    // ZIP64 (APPNOTE 4.3.14-4.3.15): a locator right before the end record points at the zip64 end
    // record, whose counts and offsets replace the 16- and 32-bit ones. Streaming writers add it even to small archives.
    if eocd >= 20 && src.u32_at(eocd - 20)? == 0x07064b50 {
        let r = src.u64_at(eocd - 12)?;
        if len < 56 || r > (len - 56) as u64 || src.u32_at(r as usize)? != 0x06064b50 {
            return Err(bad("corrupt zip64 end-of-central-directory record".into()));
        }
        let r = r as usize;
        count = src.u64_at(r + 32)?;
        p0 = src.u64_at(r + 48)?;
    }
    if count > limits.max_entries {
        return Err(limit(
            origin,
            format!("archive has more than {} entries", limits.max_entries),
            span,
        ));
    }

    let mut list: Vec<ZipEntry> = Vec::new();
    let mut declared: u64 = 0;
    // Past the end: any offset that cannot hold a record.
    let mut p = usize::try_from(p0).unwrap_or(usize::MAX).min(len);
    for _ in 0..count {
        if p + 46 > len || src.u32_at(p)? != 0x02014b50 {
            return Err(bad("corrupt zip central directory".into()));
        }
        let name_len = src.u16_at(p + 28)? as usize;
        let extra_len = src.u16_at(p + 30)? as usize;
        let advance = 46 + name_len + extra_len + src.u16_at(p + 32)? as usize;
        if p + advance > len {
            return Err(bad("corrupt zip central directory".into()));
        }
        let unix = src.u16_at(p + 4)? >> 8 == 3;
        let attr = src.u32_at(p + 38)? >> 16;
        let file_type = attr & 0o170000;
        let flags = src.u16_at(p + 8)?;
        let raw_name = src.read(p + 46, name_len)?.into_owned();
        let extra = src.read(p + 46 + name_len, extra_len)?;
        let name = zip_name(&raw_name, flags, &extra);
        let mut e = ZipEntry {
            raw_name,
            method: src.u16_at(p + 10)?,
            crc: src.u32_at(p + 16)?,
            csize: src.u32_at(p + 20)? as u64,
            usize_: src.u32_at(p + 24)? as u64,
            local: src.u32_at(p + 42)? as u64,
            mode: if unix { attr & 0o7777 } else { 0o644 },
            // Some tools mark directories only by mode, without a trailing slash.
            is_dir: name.ends_with('/') || (unix && file_type == 0o040000),
            name,
        };
        if flags & 1 != 0 {
            return Err(bad(format!(
                "encrypted zip entry '{}' is not supported",
                e.name
            )));
        }
        // ZIP64 extra field (0x0001): the 64-bit values of the fields that are 0xffffffff, in this order.
        const MAX: u64 = 0xffff_ffff;
        if e.usize_ == MAX || e.csize == MAX || e.local == MAX {
            let z = extra_field(&extra, 0x0001);
            let mut at = 0usize;
            let name = e.name.clone();
            let mut next = || -> Result<u64> {
                match z {
                    Some(z) if at + 8 <= z.len() => {
                        at += 8;
                        Ok(u64le(z, at - 8))
                    }
                    _ => Err(bad(format!("corrupt zip64 extra field for '{name}'"))),
                }
            };
            if e.usize_ == MAX {
                e.usize_ = next()?;
            }
            if e.csize == MAX {
                e.csize = next()?;
            }
            if e.local == MAX {
                e.local = next()?;
            }
        }
        if unix && file_type != 0 && file_type != 0o100000 && file_type != 0o040000 {
            return Err(SitefileError::new(
                E_ARCHIVE_UNSAFE,
                format!(
                    "unsafe archive entry '{}': symlink or special file entries are not allowed",
                    e.name
                ),
                span.cloned(),
            ));
        }
        declared = declared.saturating_add(e.usize_);
        list.push(e);
        p += advance;
    }
    if declared > limits.max_total_bytes {
        return Err(limit(
            origin,
            format!("archive expands beyond {} bytes", limits.max_total_bytes),
            span,
        ));
    }
    if declared
        > (len as u64)
            .saturating_mul(limits.max_ratio)
            .max(RATIO_FLOOR)
    {
        return Err(limit(
            origin,
            format!("compression ratio exceeds {}:1", limits.max_ratio),
            span,
        ));
    }

    // Every local header must agree with its central record, and no two entries' bytes may overlap:
    // otherwise other tools (unzip, browsers, Archive Utility) would see different files (#184).
    let unsafe_ = |e: &ZipEntry, why: &str| {
        SitefileError::new(
            E_ARCHIVE_UNSAFE,
            format!("unsafe archive entry '{}': {why}", e.name),
            span.cloned(),
        )
    };
    // (index into list, local header offset, data start, data end)
    let mut spans: Vec<(usize, usize, usize, usize)> = Vec::with_capacity(list.len());
    for (i, e) in list.iter().enumerate() {
        // Past the end: any offset that cannot hold a header.
        let h = usize::try_from(e.local).unwrap_or(usize::MAX).min(len);
        if h + 30 > len || src.u32_at(h)? != 0x04034b50 {
            return Err(bad(format!("corrupt zip local header for '{}'", e.name)));
        }
        let name_len = src.u16_at(h + 26)? as usize;
        let start = h + 30 + name_len + src.u16_at(h + 28)? as usize;
        if start as u64 + e.csize > len as u64 {
            return Err(bad(format!("truncated zip entry '{}'", e.name)));
        }
        if *src.read(h + 30, name_len)? != *e.raw_name {
            return Err(unsafe_(e, "its local header has a different name"));
        }
        if src.u16_at(h + 8)? != e.method {
            return Err(unsafe_(
                e,
                "its local header has a different compression method",
            ));
        }
        // With a data descriptor (bit 3) the local sizes may be zero; the descriptor follows the data.
        if src.u16_at(h + 6)? & 8 == 0 {
            let mut csize = src.u32_at(h + 18)? as u64;
            let mut usize_ = src.u32_at(h + 22)? as u64;
            if csize == 0xffff_ffff || usize_ == 0xffff_ffff {
                // A local ZIP64 extra field holds both sizes (APPNOTE 4.5.3); Python's force_zip64 writes one.
                let local_extra = src.read(h + 30 + name_len, start - (h + 30 + name_len))?;
                if let Some(z) = extra_field(&local_extra, 0x0001) {
                    if z.len() >= 16 {
                        (usize_, csize) = (u64le(z, 0), u64le(z, 8));
                    }
                }
            }
            if csize != e.csize || usize_ != e.usize_ {
                return Err(unsafe_(e, "its local header has different sizes"));
            }
        }
        spans.push((i, h, start, start + e.csize as usize));
    }
    let mut by_offset = spans.clone();
    by_offset.sort_by_key(|s| s.1);
    for w in by_offset.windows(2) {
        let (prev, cur) = (w[0], w[1]);
        if cur.1 < prev.3 {
            return Err(unsafe_(
                &list[cur.0],
                &format!("its data overlaps entry '{}'", list[prev.0].name),
            ));
        }
    }
    let starts: Vec<usize> = spans.iter().map(|s| s.2).collect();
    Ok(list
        .into_iter()
        .zip(starts)
        .map(|(e, start)| ZipSpan { e, start })
        .collect())
}

/// Check an inflated (or stored) entry against its header: size and CRC-32.
fn check_zip_data(data: &[u8], e: &ZipEntry, bad: &dyn Fn(String) -> SitefileError) -> Result<()> {
    if data.len() as u64 != e.usize_ {
        return Err(bad(format!(
            "zip entry '{}' size does not match its header",
            e.name
        )));
    }
    // Inflate cannot notice a corrupt stored entry; the CRC-32 can.
    let mut crc = flate2::Crc::new();
    crc.update(data);
    if crc.sum() != e.crc {
        return Err(bad(format!(
            "zip entry '{}' fails its CRC-32 check",
            e.name
        )));
    }
    Ok(())
}

/// An entry's bytes, inflated (never beyond its declared size) and checked.
fn zip_data(
    src: &dyn ZipSource,
    s: &ZipSpan,
    origin: &str,
    limits: &ArchiveLimits,
    span: Option<&SourceSpan>,
) -> Result<Vec<u8>> {
    let bad = |why: String| corrupt(origin, why, span);
    let e = &s.e;
    let raw = src.read(s.start, e.csize as usize)?;
    let data: Vec<u8> = match e.method {
        0 => raw.into_owned(),
        8 => {
            // Headers can lie: never inflate more than the declared size.
            match inflate_like_node(&raw, false, e.usize_.max(1)) {
                Inflated::Done(d) => d,
                Inflated::TooLarge => return Err(too_large(origin, false, limits, span)),
                Inflated::Corrupt => return Err(bad("corrupt compressed data".into())),
            }
        }
        m => {
            return Err(bad(format!(
                "zip entry '{}' uses unsupported compression method {m}",
                e.name
            )))
        }
    };
    check_zip_data(&data, e, &bad)?;
    Ok(data)
}

fn read_zip(b: &[u8], c: &mut Collector) -> Result<()> {
    let (origin, span, limits) = (c.origin, c.span, *c.limits);
    let src = MemorySource(b);
    for s in zip_index(&src, origin, &limits, span)? {
        if s.e.is_dir {
            c.add(&s.e.name, Kind::Dir, &[], 0o755)?;
            continue;
        }
        let data = zip_data(&src, &s, origin, &limits, span)?;
        c.add(&s.e.name, Kind::File, &data, s.e.mode)?;
    }
    Ok(())
}

/// A zip entry's raw deflate stream, for a gzip response ([`ZipFile::deflated`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deflated {
    pub deflate: Vec<u8>,
    pub crc: u32,
    pub size: u64,
}

struct FileSource {
    file: std::fs::File,
    size: usize,
    origin: String,
}

impl ZipSource for FileSource {
    fn size(&self) -> usize {
        self.size
    }
    fn read(&self, at: usize, len: usize) -> Result<Cow<'_, [u8]>> {
        use std::os::unix::fs::FileExt;
        let mut buf = vec![0u8; len];
        self.file.read_exact_at(&mut buf, at as u64).map_err(|_| {
            SitefileError::plain(
                E_ARCHIVE,
                format!("{}: the file changed while it was read", self.origin),
            )
        })?;
        Ok(Cow::Owned(buf))
    }
}

/// A zip file served without holding its contents ([`open_zip_file`]).
pub struct ZipFile {
    /// The archive's files; their `bytes` are empty (read them with [`ZipFile::read`]), digests and modes are real.
    pub tree: Tree,
    src: FileSource,
    by_path: std::collections::HashMap<String, ZipSpan>,
}

impl ZipFile {
    /// The bytes of a file of `tree`, read from the zip again and checked; `None` for other paths.
    pub fn read(&self, path: &str) -> Option<Result<Vec<u8>>> {
        let s = self.by_path.get(path)?;
        let changed = || {
            SitefileError::plain(
                E_ARCHIVE,
                format!(
                    "{}: zip entry '{}' changed since the server started",
                    self.src.origin, s.e.name
                ),
            )
        };
        Some((|| {
            let raw = self.src.read(s.start, s.e.csize as usize)?;
            let data = if s.e.method == 8 {
                match inflate_like_node(&raw, false, s.e.usize_.max(1)) {
                    Inflated::Done(d) => d,
                    _ => return Err(changed()),
                }
            } else {
                raw.into_owned()
            };
            check_zip_data(&data, &s.e, &|_| changed())?;
            Ok(data)
        })())
    }

    /// A deflated file's raw deflate stream with its CRC-32 and size (for gzip responses).
    pub fn deflated(&self, path: &str) -> Option<Deflated> {
        let s = self.by_path.get(path)?;
        if s.e.method != 8 {
            return None;
        }
        let deflate = self
            .src
            .read(s.start, s.e.csize as usize)
            .ok()?
            .into_owned();
        Some(Deflated {
            deflate,
            crc: s.e.crc,
            size: s.e.usize_,
        })
    }
}

/// Open a zip file for serving without extracting it into memory: the central
/// directory and local headers are read and checked as [`extract_archive`]
/// checks them, and every entry is inflated once (one at a time) for its
/// CRC-32, digest and the limits. Afterwards only the entry index is held;
/// [`ZipFile::read`] reads an entry from the file on demand.
pub fn open_zip_file(
    file: &std::path::Path,
    origin: &str,
    limits: &ArchiveLimits,
) -> Result<ZipFile> {
    let unreadable = || SitefileError::plain(E_ARCHIVE, format!("cannot read '{origin}'"));
    let f = std::fs::File::open(file).map_err(|_| unreadable())?;
    let size = f.metadata().map_err(|_| unreadable())?.len() as usize;
    let src = FileSource {
        file: f,
        size,
        origin: origin.to_string(),
    };
    let mut c = Collector {
        origin,
        limits,
        span: None,
        files: Vec::new(),
        file_paths: HashSet::new(),
        dirs: HashSet::new(),
        total: 0,
        retain: false,
    };
    let mut by_path = std::collections::HashMap::new();
    for s in zip_index(&src, origin, limits, None)? {
        if s.e.is_dir {
            c.add(&s.e.name, Kind::Dir, &[], 0o755)?;
            continue;
        }
        let data = zip_data(&src, &s, origin, limits, None)?;
        c.add(&s.e.name, Kind::File, &data, s.e.mode)?;
        by_path.insert(archive_path(&s.e.name, None)?, s);
    }
    Ok(ZipFile {
        tree: Tree::from_files(c.files)?,
        src,
        by_path,
    })
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
        TarEntry {
            name: name.into(),
            data: data.as_bytes().to_vec(),
            ..Default::default()
        }
    }
    fn tt(name: &str, ty: u8) -> TarEntry {
        TarEntry {
            name: name.into(),
            ty: Some(ty),
            ..Default::default()
        }
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
        put(
            &mut h,
            100,
            octal(e.mode.unwrap_or(0o644) as u64, 8).as_bytes(),
            8,
        );
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
        ZipIn {
            name: name.into(),
            data: data.as_bytes().to_vec(),
            ..Default::default()
        }
    }

    pub(crate) fn zip(entries: &[ZipIn]) -> Vec<u8> {
        let mut local = Vec::new();
        let mut central = Vec::new();
        let mut offset: u32 = 0;
        for e in entries {
            let body = if e.deflate {
                deflate_raw(&e.data)
            } else {
                e.data.clone()
            };
            let name = e.name.as_bytes();
            let usize_ = e.fake_size.unwrap_or(e.data.len() as u32);
            let crc = {
                let mut c = flate2::Crc::new();
                c.update(&e.data);
                c.sum()
            };
            let mut l = vec![0u8; 30];
            l[14..18].copy_from_slice(&crc.to_le_bytes());
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
            c[16..20].copy_from_slice(&crc.to_le_bytes());
            c[20..24].copy_from_slice(&(body.len() as u32).to_le_bytes());
            c[24..28].copy_from_slice(&usize_.to_le_bytes());
            c[28..30].copy_from_slice(&(name.len() as u16).to_le_bytes());
            let mode = e.mode.unwrap_or(if e.name.ends_with('/') {
                0o040755
            } else {
                0o100644
            });
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
        assert!(
            e.message().contains(needle),
            "{:?} does not contain {needle:?}",
            e.message()
        );
    }

    fn sample() -> Vec<TarEntry> {
        vec![
            tt("site/", b'5'),
            te("site/index.html", "<h1>hi</h1>"),
            TarEntry {
                mode: Some(0o700),
                ..te("site/bin/run", "#!/bin/sh")
            },
            TarEntry {
                mode: Some(0o600),
                ..te("site/css/a.css", "a{}")
            },
        ]
    }

    // ---------------------------------------------------------------- tests

    #[test]
    fn tar_tgz_and_zip_extract_to_the_same_tree() {
        let zip_entries = vec![
            ZipIn {
                name: "site/".into(),
                mode: Some(0o040755),
                ..Default::default()
            },
            ZipIn {
                deflate: true,
                ..ze("site/index.html", "<h1>hi</h1>")
            },
            ZipIn {
                mode: Some(0o100700),
                ..ze("site/bin/run", "#!/bin/sh")
            },
            ZipIn {
                deflate: true,
                mode: Some(0o100600),
                ..ze("site/css/a.css", "a{}")
            },
        ];
        let trees = [
            x(&tar(&sample())).unwrap(),
            x(&gzip(&tar(&sample()))).unwrap(),
            x(&zip(&zip_entries)).unwrap(),
        ];
        for t in &trees {
            assert_eq!(
                paths(t),
                vec!["/site/bin/run", "/site/css/a.css", "/site/index.html"]
            );
            assert_eq!(&*t.get("/site/index.html").unwrap().bytes, b"<h1>hi</h1>");
            assert_eq!(t.get("/site/bin/run").unwrap().mode, 0o755);
            assert_eq!(t.get("/site/css/a.css").unwrap().mode, 0o644);
            assert_eq!(
                t.get("/site/index.html").unwrap().origin,
                "t:site/index.html"
            );
        }
        assert_eq!(trees[0].digest(), trees[1].digest());
        assert_eq!(trees[0].digest(), trees[2].digest());
    }

    #[test]
    fn gzip_wrapped_web_bundles_from_fileable_and_packfile() {
        // test/fixtures/gzip-wbn: written by @johnhenry/fileable (encode="wbn") and packfile's
        // toArchive(); expected.json is shared with test/archive.test.ts (johnhenry/sitefile#141).
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../test/fixtures/gzip-wbn");
        let expected: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("expected.json")).unwrap())
                .unwrap();
        let expected = expected.as_object().unwrap();
        assert_eq!(expected.len(), 2);
        for (name, want) in expected {
            let bytes = std::fs::read(dir.join(name)).unwrap();
            assert_eq!(&bytes[..2], &[0x1f, 0x8b], "{name} is gzip");
            let t = extract_archive(&bytes, name, &L, None).unwrap();
            let files: serde_json::Map<String, serde_json::Value> = t
                .entries()
                .map(|(p, n)| (p.clone(), serde_json::Value::String(n.digest.clone())))
                .collect();
            assert_eq!(&serde_json::Value::Object(files), &want["files"], "{name}");
            assert_eq!(t.digest(), want["tree"].as_str().unwrap(), "{name}");
        }
        // the bundle's own limits apply to the decompressed stream
        let bundle = std::fs::read(dir.join("packfile-toarchive.wbn")).unwrap();
        let limited =
            |l: ArchiveLimits| extract_archive(&bundle, "packfile", &l, None).unwrap_err();
        let e = limited(ArchiveLimits {
            max_entries: 2,
            ..L.clone()
        });
        assert_eq!(
            (e.code, e.message()),
            (
                ErrorCode::E_ARCHIVE_LIMIT,
                "packfile: web bundle has more than 2 entries".to_string()
            )
        );
        let e = limited(ArchiveLimits {
            max_total_bytes: 20,
            ..L.clone()
        });
        assert_eq!(
            (e.code, e.message()),
            (
                ErrorCode::E_ARCHIVE_LIMIT,
                "packfile: web bundle expands beyond 20 bytes".to_string()
            )
        );
    }

    #[test]
    fn empty_archives_and_dot_slash_prefixes() {
        assert_eq!(x(&zip(&[])).unwrap().len(), 0);
        assert_eq!(x(&tar(&[])).unwrap().len(), 0);
        assert_eq!(
            paths(&x(&tar(&[te("./a/b.txt", "x")])).unwrap()),
            vec!["/a/b.txt"]
        );
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
        let via_pax = tar(&[
            TarEntry {
                ty: Some(b'x'),
                ..te("PaxHeader", &pax_record(&format!(" path={long}\n")))
            },
            te("short", "p"),
        ]);
        assert_eq!(paths(&x(&via_pax).unwrap()), vec![format!("/{long}")]);
        let via_gnu = tar(&[
            TarEntry {
                ty: Some(b'L'),
                ..te("././@LongLink", &format!("{long}\0"))
            },
            te("short", "g"),
        ]);
        assert_eq!(paths(&x(&via_gnu).unwrap()), vec![format!("/{long}")]);
    }

    #[test]
    fn pax_size_overrides_the_header_size() {
        let data = "0123456789";
        let mut b = tar(&[TarEntry {
            ty: Some(b'x'),
            ..te("PaxHeader", &pax_record(" size=10\n"))
        }]);
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
        rejects(
            b"just some text, not an archive",
            E_TYPE,
            "not a recognized archive",
            L,
        );
        rejects(b"", E_TYPE, "not a recognized archive", L);
        rejects(&gzip(b"hello"), E_TYPE, "not a tar archive", L);
    }

    #[test]
    fn unsafe_tar_paths_are_rejected() {
        for name in [
            "../evil",
            "a/../../evil",
            "/etc/passwd",
            "a\\b",
            "C:/x",
            "a/..",
        ] {
            rejects(
                &tar(&[te(name, "x")]),
                E_ARCHIVE_UNSAFE,
                "unsafe archive entry",
                L,
            );
        }
        // A NUL cannot appear in a plain tar name (it terminates it) but can in a pax path.
        let rec = "path=a\0b\n";
        let len = rec.len() + 3;
        rejects(
            &tar(&[
                TarEntry {
                    ty: Some(b'x'),
                    ..te("p", &format!("{len} {rec}"))
                },
                te("x", "x"),
            ]),
            E_ARCHIVE_UNSAFE,
            "NUL",
            L,
        );
    }

    #[test]
    fn unsafe_zip_paths_are_rejected() {
        for name in ["../evil", "/abs", "a\\b", "x/../../y", "a\0b"] {
            rejects(
                &zip(&[ze(name, "x")]),
                E_ARCHIVE_UNSAFE,
                "unsafe archive entry",
                L,
            );
        }
    }

    #[test]
    fn symlink_hard_link_and_device_entries_are_rejected() {
        rejects(
            &tar(&[TarEntry {
                linkname: Some("/etc/passwd".into()),
                ..tt("l", b'2')
            }]),
            E_ARCHIVE_UNSAFE,
            "symlink",
            L,
        );
        rejects(
            &tar(&[TarEntry {
                linkname: Some("x".into()),
                ..tt("h", b'1')
            }]),
            E_ARCHIVE_UNSAFE,
            "hard link",
            L,
        );
        rejects(&tar(&[tt("d", b'3')]), E_ARCHIVE_UNSAFE, "special file", L);
        rejects(
            &zip(&[ZipIn {
                mode: Some(0o120777),
                ..ze("l", "/etc/passwd")
            }]),
            E_ARCHIVE_UNSAFE,
            "symlink",
            L,
        );
    }

    #[test]
    fn too_many_entries() {
        let entries: Vec<TarEntry> = (0..6).map(|i| te(&format!("f{i}"), "x")).collect();
        let zentries: Vec<ZipIn> = (0..6).map(|i| ze(&format!("f{i}"), "x")).collect();
        let five = ArchiveLimits {
            max_entries: 5,
            ..L
        };
        rejects(&tar(&entries), E_ARCHIVE_LIMIT, "more than 5 entries", five);
        rejects(
            &gzip(&tar(&entries)),
            E_ARCHIVE_LIMIT,
            "more than 5 entries",
            five,
        );
        rejects(
            &zip(&zentries),
            E_ARCHIVE_LIMIT,
            "more than 5 entries",
            five,
        );
        assert_eq!(
            extract_archive(
                &tar(&entries),
                "t",
                &ArchiveLimits {
                    max_entries: 6,
                    ..L
                },
                None
            )
            .unwrap()
            .len(),
            6
        );
    }

    #[test]
    fn oversize_total() {
        let k = ArchiveLimits {
            max_total_bytes: 1000,
            ..L
        };
        let a = "x".repeat(600);
        let b = "y".repeat(600);
        rejects(
            &tar(&[te("a", &a), te("b", &b)]),
            E_ARCHIVE_LIMIT,
            "expands beyond 1000 bytes",
            k,
        );
        rejects(
            &zip(&[ze("a", &a), ze("b", &b)]),
            E_ARCHIVE_LIMIT,
            "expands beyond 1000 bytes",
            k,
        );
        rejects(
            &tar(&[te("a", &"x".repeat(5000))]),
            E_ARCHIVE_LIMIT,
            "larger than 1000 bytes",
            k,
        );
    }

    #[test]
    fn compression_ratio_bombs_stop_early() {
        // 50 MB of zeros gzips to ~50 KB (ratio ~1000:1): rejected at the default 100:1 without inflating it all.
        let zeros = TarEntry {
            name: "zeros".into(),
            data: vec![0u8; 50 * 1024 * 1024],
            ..Default::default()
        };
        let bomb = gzip(&tar(&[zeros]));
        rejects(&bomb, E_ARCHIVE_LIMIT, "compression ratio exceeds 100:1", L);
        // A low total-size limit also stops it.
        rejects(
            &bomb,
            E_ARCHIVE_LIMIT,
            "expands beyond 1048576 bytes",
            ArchiveLimits {
                max_total_bytes: 1024 * 1024,
                max_ratio: 100_000,
                ..L
            },
        );
        // Zip: honest headers over the ratio, and headers that lie about the size.
        let big = vec![0u8; 5 * 1024 * 1024];
        rejects(
            &zip(&[ZipIn {
                name: "z".into(),
                data: big.clone(),
                deflate: true,
                ..Default::default()
            }]),
            E_ARCHIVE_LIMIT,
            "ratio",
            L,
        );
        rejects(
            &zip(&[ZipIn {
                name: "z".into(),
                data: big,
                deflate: true,
                fake_size: Some(10),
                ..Default::default()
            }]),
            E_ARCHIVE_LIMIT,
            "expands",
            L,
        );
    }

    #[test]
    fn duplicate_paths_are_e_conflict() {
        rejects(
            &tar(&[te("a", "1"), te("./a", "1")]),
            E_CONFLICT,
            "duplicate",
            L,
        );
        rejects(
            &zip(&[ze("a", "1"), ze("a", "2")]),
            E_CONFLICT,
            "duplicate",
            L,
        );
        rejects(&tar(&[te("a", "1"), te("a/b", "2")]), E_CONFLICT, "", L);
        rejects(
            &tar(&[tt("a", b'5'), te("a", "2")]),
            E_CONFLICT,
            "duplicate",
            L,
        );
    }

    #[test]
    fn corrupt_archives_are_e_archive() {
        let mut bad_sum = tar(&[te("a", "x")]);
        bad_sum[0] = 0x7a;
        rejects(&bad_sum, E_ARCHIVE, "checksum", L);
        let mut trunc = tar_header(&te("big", ""), 4096);
        trunc.extend(vec![0u8; 100]);
        rejects(&trunc, E_ARCHIVE, "truncated", L);
        rejects(
            &zip(&[ze("a", "x")])[..30],
            E_ARCHIVE,
            "end-of-central-directory",
            L,
        );
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
        let t = extract_archive(
            &gzip(&tar(&entries)),
            "t",
            &ArchiveLimits {
                max_total_bytes: 4000,
                max_entries: 2000,
                max_ratio: 1_000_000,
            },
            None,
        )
        .unwrap();
        assert_eq!(t.len(), 2000);
    }

    #[test]
    fn zip_directories_marked_only_by_mode_are_directories() {
        let z = zip(&[
            ZipIn {
                name: "assets".into(),
                mode: Some(0o040755),
                ..Default::default()
            },
            ze("assets/app.css", "a{}"),
        ]);
        assert_eq!(paths(&x(&z).unwrap()), vec!["/assets/app.css"]);
    }

    #[test]
    fn malformed_pax_records_are_e_archive() {
        let pax = |body: &str| {
            tar(&[
                TarEntry {
                    ty: Some(b'x'),
                    ..te("PaxHeader", &pax_record(body))
                },
                te("a", "x"),
            ])
        };
        rejects(
            &pax(" size=abc\n"),
            E_ARCHIVE,
            "malformed pax size 'abc'",
            L,
        );
        rejects(
            &pax(" novalue\n"),
            E_ARCHIVE,
            "malformed pax header record",
            L,
        );
    }

    #[test]
    fn a_pax_record_without_a_space_ends_the_header() {
        let t = x(&tar(&[
            TarEntry {
                ty: Some(b'x'),
                ..te("PaxHeader", "10 path=p\ntail")
            },
            te("a", "x"),
        ]))
        .unwrap();
        assert_eq!(paths(&t), ["/p"]);
        let t = x(&tar(&[
            TarEntry {
                ty: Some(b'x'),
                ..te("PaxHeader", "nospace")
            },
            te("a", "x"),
        ]))
        .unwrap();
        assert_eq!(paths(&t), ["/a"]);
    }

    #[test]
    fn tar_numeric_fields_are_read_like_parse_int() {
        let entry = |size: &[u8], mode: &[u8]| {
            let mut h = tar_header(&te("f", ""), 3);
            h[124..136].fill(0);
            h[124..124 + size.len()].copy_from_slice(size);
            h[100..108].fill(0);
            h[100..100 + mode.len()].copy_from_slice(mode);
            with_checksum(&mut h);
            h.extend(b"abc");
            h.extend(vec![0u8; 509 + 1024]);
            h
        };
        let t = x(&entry(b" \xa0+3", b"\x0b-5")).unwrap();
        let f = t.get("/f").unwrap();
        assert_eq!(&*f.bytes, b"abc");
        assert_eq!(f.mode, 0o755); // -5 & 0o111 is non-zero
        rejects(
            &entry(b"-3", b"644"),
            E_ARCHIVE_LIMIT,
            "archive entry larger than",
            L,
        );
    }

    #[test]
    fn trailing_zeros_after_a_gzip_member_are_ignored() {
        let gz = gzip(&tar(&[te("a", "x")]));
        let mut padded = gz.clone();
        padded.extend([0u8; 9]);
        assert_eq!(paths(&x(&padded).unwrap()), ["/a"]);
        let mut junk = gz;
        junk.extend([1u8, 2, 3]);
        rejects(&junk, E_ARCHIVE, "corrupt compressed data", L);
    }

    #[test]
    fn an_inflate_error_wins_over_output_not_yet_handed_on() {
        // 20 000 bytes with a bad CRC: Node reports the CRC error unless a full
        // 16 KiB buffer was already over the limit.
        let data: Vec<u8> = (0..20_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let mut gz = gzip(&data);
        let n = gz.len();
        gz[n - 6] ^= 1;
        assert!(matches!(
            inflate_like_node(&gz, true, 16_384),
            Inflated::Corrupt
        ));
        assert!(matches!(
            inflate_like_node(&gz, true, 16_383),
            Inflated::TooLarge
        ));
        // Truncated: everything inflated so far counts.
        let gz = gzip(&data);
        assert!(matches!(
            inflate_like_node(&gz[..gz.len() - 4], true, 19_999),
            Inflated::TooLarge
        ));
        assert!(matches!(
            inflate_like_node(&gz[..gz.len() - 4], true, 20_000),
            Inflated::Corrupt
        ));
    }

    /// What Finder's "Compress" makes of a folder `site` (plus Windows' and Finder's per-folder files).
    pub(crate) fn finder_zip() -> Vec<u8> {
        let dir = |n: &str| ZipIn {
            name: n.into(),
            mode: Some(0o040755),
            ..Default::default()
        };
        zip(&[
            dir("site/"),
            ZipIn {
                deflate: true,
                ..ze("site/index.html", "<h1>hi</h1>")
            },
            ze("site/.DS_Store", "Bud1"),
            ze("site/Icon\r", ""),
            ze("site/img/Thumbs.db", "x"),
            ze("site/img/Desktop.ini", "x"),
            ze("site/img/a.png", "png"),
            dir("__MACOSX/"),
            dir("__MACOSX/site/"),
            ze("__MACOSX/site/._index.html", "Mac OS X"),
            ze("__MACOSX/site/img/._a.png", "Mac OS X"),
        ])
    }

    #[test]
    fn os_metadata_is_recognized() {
        for p in [
            "/__MACOSX/a",
            "/x/__MACOSX/y",
            "/._a.html",
            "/d/._x",
            "/.DS_Store",
            "/d/Icon\r",
            "/THUMBS.DB",
            "/d/Desktop.ini",
        ] {
            assert!(is_os_metadata(p), "{p:?}");
        }
        for p in [
            "/index.html",
            "/_a",
            "/.well-known/x",
            "/Icon",
            "/thumbs.db.bak",
            "/__MACOSX.txt",
            "/a.DS_Store",
            "/.ds_store_x",
        ] {
            assert!(!is_os_metadata(p), "{p:?}");
        }
    }

    #[test]
    fn a_finder_zip_loses_its_os_metadata_and_is_a_single_folder() {
        let (tree, skipped) = without_os_metadata(x(&finder_zip()).unwrap()).unwrap();
        assert_eq!(skipped, 6);
        assert_eq!(paths(&tree), ["/site/img/a.png", "/site/index.html"]);
        assert_eq!(
            os_metadata_warning("IMPORT s", 6),
            "IMPORT s: skipped 6 OS metadata files (__MACOSX, ._*, .DS_Store, Thumbs.db, desktop.ini, Icon\\r); --keep-os-metadata keeps them"
        );
    }

    #[test]
    fn a_flipped_byte_in_a_stored_entry_fails_the_crc_check() {
        let mut z = zip(&[ze("index.html", "<h1>hello</h1>")]);
        assert!(x(&z).is_ok());
        z[30 + 10 + 4] ^= 0x20; // 'h' -> 'H'
        rejects(&z, E_ARCHIVE, "'index.html' fails its CRC-32 check", L);
        // A bad CRC on an empty entry too.
        let mut e = zip(&[ze("empty", "")]);
        let n = e.len();
        e[n - 22 - 51 + 16] ^= 1; // the central CRC
        rejects(&e, E_ARCHIVE, "'empty' fails its CRC-32 check", L);
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

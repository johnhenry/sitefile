//! Deterministic archive writers (port of `src/archive-writers.ts`). The same
//! tree always yields byte-identical output: entries are sorted by path, there
//! are no directory entries (except the explicit tar `dirs`), uid/gid are 0,
//! there are no user/group names, modes are the tree's 0644/0755, and every
//! timestamp is fixed.
//!
//! Tar output, stored zip entries and every header field match the TypeScript
//! writer byte for byte. Deflated zip entries and gzip streams come from
//! flate2 instead of Node's zlib, so their compressed bytes can differ.

use crate::error::{ErrorCode::*, Result, SitefileError};
use crate::tree::Tree;
use flate2::write::{DeflateEncoder, GzEncoder};
use flate2::{Compression, GzBuilder};
use std::io::Write;

#[derive(Debug, Clone, Default)]
pub struct WriterOptions {
    /// Seconds since the Unix epoch for every entry timestamp (`SOURCE_DATE_EPOCH`).
    /// Default: 1980-01-01 00:00 for zip (the DOS epoch) and 0 for tar. The
    /// gzip header mtime is always 0.
    pub epoch: Option<u64>,
    /// tar only: directory entries (relative, no slashes at the ends, e.g.
    /// `srv`, `srv/www`) written before the files.
    pub dirs: Vec<String>,
}

/// Read `SOURCE_DATE_EPOCH` from the process environment.
pub fn source_date_epoch() -> Result<Option<u64>> {
    source_date_epoch_from(std::env::var("SOURCE_DATE_EPOCH").ok().as_deref())
}

/// `None` if unset or empty, `E_USAGE` if malformed.
pub fn source_date_epoch_from(raw: Option<&str>) -> Result<Option<u64>> {
    match raw {
        None | Some("") => Ok(None),
        Some(r) if r.len() <= 12 && r.bytes().all(|b| b.is_ascii_digit()) => Ok(Some(r.parse().unwrap_or(0))),
        Some(r) => Err(SitefileError::plain(E_USAGE, format!("SOURCE_DATE_EPOCH must be a non-negative integer, got '{r}'"))),
    }
}

struct Entry<'a> {
    name: &'a str,
    bytes: &'a [u8],
    mode: u32,
}

fn entries_of(tree: &Tree) -> Result<Vec<Entry<'_>>> {
    tree.entries()
        .map(|(p, n)| {
            let bad = !p.starts_with('/')
                || p.ends_with('/')
                || p.contains('\0')
                || p.contains('\\')
                || p.split('/').skip(1).any(|s| s.is_empty() || s == "." || s == "..");
            if bad {
                return Err(SitefileError::plain(E_EXPORT, format!("invalid artifact path '{p}'")));
            }
            Ok(Entry { name: &p[1..], bytes: &n.bytes, mode: n.mode })
        })
        .collect()
}

// ------------------------------------------------------------------ CRC-32

const fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[n] = c;
        n += 1;
    }
    t
}
static CRC_TABLE: [u32; 256] = crc_table();

pub fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xffff_ffffu32;
    for &b in data {
        c = CRC_TABLE[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
    }
    c ^ 0xffff_ffff
}

// ------------------------------------------------------------------- dates

/// (year, month 1-12, day, hour, minute, second) of a Unix time, UTC.
pub(crate) fn civil(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's days_from_civil inverse.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, (rem / 3600) as u32, ((rem % 3600) / 60) as u32, (rem % 60) as u32)
}

/// `Date.prototype.toISOString()` for a time in milliseconds.
pub(crate) fn iso_millis(ms: i64) -> String {
    let (y, mo, d, h, mi, s) = civil(ms.div_euclid(1000));
    let year = if (0..=9999).contains(&y) { format!("{y:04}") } else if y < 0 { format!("-{:06}", -y) } else { format!("+{y:06}") };
    format!("{year}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{:03}Z", ms.rem_euclid(1000))
}

// --------------------------------------------------------------------- zip

const ZIP_MAX_ENTRIES: usize = 0xffff;
const ZIP_MAX: u64 = 0xffff_ffff;

fn dos_date_time(epoch: Option<u64>) -> (u16, u16) {
    let Some(e) = epoch else { return (0, (1 << 5) | 1) }; // 1980-01-01 00:00:00
    let (y, mo, d, h, mi, s) = civil(e.max(315_532_800) as i64); // clamp to 1980-01-01
    let year = y.min(2107);
    // Seconds are stored in 2-second units.
    let time = ((h << 11) | (mi << 5) | (s >> 1)) as u16;
    let date = ((((year - 1980) as u32) << 9) | (mo << 5) | d) as u16;
    (time, date)
}

fn deflate_raw(data: &[u8]) -> Vec<u8> {
    let mut e = DeflateEncoder::new(Vec::with_capacity(data.len() / 2 + 16), Compression::best());
    // Writing to a Vec cannot fail.
    let _ = e.write_all(data);
    e.finish().unwrap_or_default()
}

/// Zip (no zip64). Deflate when it shrinks the data, otherwise stored.
pub fn write_zip(tree: &Tree, options: &WriterOptions) -> Result<Vec<u8>> {
    let entries = entries_of(tree)?;
    if entries.len() > ZIP_MAX_ENTRIES {
        return Err(SitefileError::plain(
            E_EXPORT,
            format!("zip export supports at most {ZIP_MAX_ENTRIES} entries (zip64 is not implemented); got {}", entries.len()),
        ));
    }
    let (time, date) = dos_date_time(options.epoch);
    let mut out: Vec<u8> = Vec::new();
    let mut central: Vec<u8> = Vec::new();
    for e in &entries {
        let name = e.name.as_bytes();
        if name.len() > 0xffff {
            let short: String = e.name.encode_utf16().take(60).collect::<Vec<u16>>().iter().map(|&u| char::from_u32(u as u32).unwrap_or('\u{fffd}')).collect();
            return Err(SitefileError::plain(E_EXPORT, format!("zip entry name is too long ({} bytes): '{short}…'", name.len())));
        }
        let deflated = deflate_raw(e.bytes);
        let stored = deflated.len() >= e.bytes.len();
        let data: &[u8] = if stored { e.bytes } else { &deflated };
        if e.bytes.len() as u64 > ZIP_MAX || data.len() as u64 > ZIP_MAX {
            return Err(SitefileError::plain(
                E_EXPORT,
                format!("zip export does not support files of 4 GiB or more (zip64 is not implemented): '/{}'", e.name),
            ));
        }
        let offset = out.len() as u64;
        if offset > ZIP_MAX {
            return Err(SitefileError::plain(E_EXPORT, "zip export does not support archives of 4 GiB or more (zip64 is not implemented)"));
        }
        let method: u16 = if stored { 0 } else { 8 };
        let crc = crc32(e.bytes);

        out.extend(0x0403_4b50u32.to_le_bytes());
        out.extend(20u16.to_le_bytes()); // version needed
        out.extend(0x0800u16.to_le_bytes()); // UTF-8 names
        out.extend(method.to_le_bytes());
        out.extend(time.to_le_bytes());
        out.extend(date.to_le_bytes());
        out.extend(crc.to_le_bytes());
        out.extend((data.len() as u32).to_le_bytes());
        out.extend((e.bytes.len() as u32).to_le_bytes());
        out.extend((name.len() as u16).to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend_from_slice(name);
        out.extend_from_slice(data);

        central.extend(0x0201_4b50u32.to_le_bytes());
        central.extend(((3u16 << 8) | 20).to_le_bytes()); // made by: Unix, spec 2.0
        central.extend(20u16.to_le_bytes());
        central.extend(0x0800u16.to_le_bytes());
        central.extend(method.to_le_bytes());
        central.extend(time.to_le_bytes());
        central.extend(date.to_le_bytes());
        central.extend(crc.to_le_bytes());
        central.extend((data.len() as u32).to_le_bytes());
        central.extend((e.bytes.len() as u32).to_le_bytes());
        central.extend((name.len() as u16).to_le_bytes());
        central.extend([0u8; 8]); // extra, comment, disk number, internal attrs
        central.extend(((0o100000 | e.mode) << 16).to_le_bytes());
        central.extend((offset as u32).to_le_bytes());
        central.extend_from_slice(name);
    }
    let offset = out.len() as u64;
    let cd_size = central.len() as u64;
    if offset + cd_size > ZIP_MAX {
        return Err(SitefileError::plain(E_EXPORT, "zip export does not support archives of 4 GiB or more (zip64 is not implemented)"));
    }
    out.extend(central);
    out.extend(0x0605_4b50u32.to_le_bytes());
    out.extend([0u8; 4]);
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend((cd_size as u32).to_le_bytes());
    out.extend((offset as u32).to_le_bytes());
    out.extend([0u8; 2]);
    Ok(out)
}

// --------------------------------------------------------------------- tar

const BLOCK: usize = 512;
const TAR_MAX_SIZE: u64 = 8u64.pow(11) - 1; // 11 octal digits

fn put_octal(h: &mut [u8], at: usize, width: usize, value: u64) {
    let s = format!("{:0>w$o}\0", value, w = width - 1);
    h[at..at + s.len()].copy_from_slice(s.as_bytes());
}

/// Largest prefix of `s` (UTF-8) not longer than `max` bytes, cut on a character boundary.
fn utf8_head(s: &str, max: usize) -> &[u8] {
    let b = s.as_bytes();
    if b.len() <= max {
        return b;
    }
    let mut end = max;
    while end > 0 && (b[end] & 0xc0) == 0x80 {
        end -= 1;
    }
    &b[..end]
}

fn tar_header(name: &[u8], prefix: &[u8], size: u64, mode: u32, mtime: u64, typ: u8) -> [u8; BLOCK] {
    let mut h = [0u8; BLOCK];
    let n = name.len().min(100);
    h[..n].copy_from_slice(&name[..n]);
    put_octal(&mut h, 100, 8, mode as u64);
    put_octal(&mut h, 108, 8, 0); // uid
    put_octal(&mut h, 116, 8, 0); // gid
    put_octal(&mut h, 124, 12, size);
    put_octal(&mut h, 136, 12, mtime);
    h[148..156].fill(0x20); // checksum placeholder
    h[156] = typ;
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    let p = prefix.len().min(155);
    h[345..345 + p].copy_from_slice(&prefix[..p]);
    let sum: u64 = h.iter().map(|&b| b as u64).sum();
    let s = format!("{sum:06o}\0 ");
    h[148..148 + s.len()].copy_from_slice(s.as_bytes());
    h
}

fn push_padded(out: &mut Vec<u8>, data: &[u8]) {
    out.extend_from_slice(data);
    let rem = data.len() % BLOCK;
    if rem != 0 {
        out.resize(out.len() + BLOCK - rem, 0);
    }
}

fn pax_record(key: &str, value: &str) -> Vec<u8> {
    let body = format!(" {key}={value}\n");
    // The length counts its own decimal digits.
    let mut len = body.len() + 1;
    while len.to_string().len() + body.len() != len {
        len = len.to_string().len() + body.len();
    }
    format!("{len}{body}").into_bytes()
}

/// Split a name for the ustar name/prefix fields, or `None` if it cannot fit.
fn ustar_split(name: &[u8]) -> Option<(&[u8], &[u8])> {
    if name.len() <= 100 {
        return Some((name, &[]));
    }
    for i in 0..name.len() {
        if name[i] != b'/' {
            continue;
        }
        let (prefix, rest) = (&name[..i], &name[i + 1..]);
        if prefix.len() > 155 {
            break;
        }
        if !rest.is_empty() && rest.len() <= 100 {
            return Some((rest, prefix));
        }
    }
    None
}

/// POSIX ustar; long names use the prefix field, then a pax `path` extended header.
pub fn write_tar(tree: &Tree, options: &WriterOptions) -> Result<Vec<u8>> {
    let mtime = options.epoch.unwrap_or(0);
    let entries = entries_of(tree)?;
    let mut out = Vec::with_capacity(entries.iter().map(|e| e.bytes.len() + 2 * BLOCK).sum::<usize>() + 2 * BLOCK);
    let mut n = 0usize;
    for d in &options.dirs {
        out.extend(tar_header(format!("{d}/").as_bytes(), &[], 0, 0o755, mtime, b'5'));
    }
    for e in &entries {
        if e.bytes.len() as u64 > TAR_MAX_SIZE {
            return Err(SitefileError::plain(E_EXPORT, format!("tar export does not support files of 8 GiB or more: '/{}'", e.name)));
        }
        let (name, prefix) = match ustar_split(e.name.as_bytes()) {
            Some(s) => s,
            None => {
                let rec = pax_record("path", e.name);
                let pname = format!("PaxHeaders/{n}");
                n += 1;
                out.extend(tar_header(pname.as_bytes(), &[], rec.len() as u64, 0o644, mtime, b'x'));
                push_padded(&mut out, &rec);
                (utf8_head(e.name, 100), &[][..])
            }
        };
        out.extend(tar_header(name, prefix, e.bytes.len() as u64, e.mode, mtime, b'0'));
        push_padded(&mut out, e.bytes);
    }
    out.resize(out.len() + 2 * BLOCK, 0);
    Ok(out)
}

/// gzip of the tar, header mtime 0 and no filename, so the stream is reproducible.
pub fn write_tgz(tree: &Tree, options: &WriterOptions) -> Result<Vec<u8>> {
    Ok(gzip_tar(&write_tar(tree, options)?))
}

/// Reproducible gzip of an already-written tar.
pub fn gzip_tar(tar: &[u8]) -> Vec<u8> {
    let mut e: GzEncoder<Vec<u8>> = GzBuilder::new().mtime(0).write(Vec::with_capacity(tar.len() / 3 + 64), Compression::best());
    let _ = e.write_all(tar);
    let mut gz = e.finish().unwrap_or_default();
    if gz.len() > 9 {
        gz[9] = 3; // OS = Unix, whatever platform zlib was built for
    }
    gz
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::tree::{make_file, Node};
    use flate2::read::{DeflateDecoder, GzDecoder};
    use std::io::Read;

    pub fn mk(files: &[(&str, &[u8], u32)]) -> Tree {
        Tree::from_files(files.iter().map(|(p, b, m)| (p.to_string(), make_file(b.to_vec(), "test", *m)))).unwrap()
    }

    pub fn sample() -> Tree {
        let blob: Vec<u8> = (0..200u32).map(|i| ((i * 37) % 256) as u8).collect();
        // String.fromCharCode + TextEncoder in the TS test: bytes >= 0x80 become two UTF-8 bytes.
        let blob = blob.iter().map(|&b| b as char).collect::<String>().into_bytes();
        mk(&[
            ("/index.html", "<!doctype html><title>x</title><p>hello</p>".repeat(20).as_bytes(), 0o644),
            ("/css/site.css", "body { color: red; }\n".repeat(50).as_bytes(), 0o644),
            ("/bin/run.sh", b"#!/bin/sh\necho hi\n", 0o755),
            ("/img/blob.bin", &blob, 0o644),
            ("/empty.txt", b"", 0o644),
            ("/ünï/cødé.txt", b"unicode name", 0o644),
        ])
    }

    /// Test-only tar reader: (path, bytes, mode) of regular files, honoring prefix and pax path.
    pub fn read_tar(b: &[u8]) -> Tree {
        let mut files: Vec<(String, Node)> = Vec::new();
        let mut off = 0;
        let mut next: Option<String> = None;
        let cstr = |s: &[u8]| String::from_utf8(s.iter().take_while(|&&c| c != 0).copied().collect()).unwrap();
        while off + 512 <= b.len() {
            let h = &b[off..off + 512];
            if h.iter().all(|&x| x == 0) {
                break;
            }
            let size = u64::from_str_radix(cstr(&h[124..136]).trim(), 8).unwrap() as usize;
            let data = &b[off + 512..off + 512 + size];
            off += 512 + size.div_ceil(512) * 512;
            match h[156] {
                b'x' => {
                    let s = String::from_utf8(data.to_vec()).unwrap();
                    let kv = s.split_once(' ').unwrap().1.trim_end_matches('\n');
                    next = Some(kv.strip_prefix("path=").unwrap().to_string());
                }
                b'5' => {}
                b'0' => {
                    let mut name = cstr(&h[0..100]);
                    let prefix = cstr(&h[345..500]);
                    if !prefix.is_empty() {
                        name = format!("{prefix}/{name}");
                    }
                    let name = next.take().unwrap_or(name);
                    let mode = u32::from_str_radix(cstr(&h[100..108]).trim(), 8).unwrap();
                    files.push((format!("/{name}"), make_file(data.to_vec(), "t", mode)));
                }
                t => panic!("unexpected type {t}"),
            }
        }
        Tree::from_files(files).unwrap()
    }

    pub fn gunzip(b: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        GzDecoder::new(b).read_to_end(&mut out).unwrap();
        out
    }

    /// Test-only zip reader via the central directory.
    pub fn read_zip(z: &[u8]) -> Tree {
        let u16at = |o: usize| u16::from_le_bytes([z[o], z[o + 1]]) as usize;
        let u32at = |o: usize| u32::from_le_bytes([z[o], z[o + 1], z[o + 2], z[o + 3]]) as usize;
        let eocd = z.len() - 22;
        let count = u16at(eocd + 10);
        let mut off = u32at(eocd + 16);
        let mut files = Vec::new();
        for _ in 0..count {
            let method = u16at(off + 10);
            let csize = u32at(off + 20);
            let usize_ = u32at(off + 24);
            let nlen = u16at(off + 28);
            let attr = (u32at(off + 38) >> 16) as u32;
            let local = u32at(off + 42);
            let name = String::from_utf8(z[off + 46..off + 46 + nlen].to_vec()).unwrap();
            let start = local + 30 + u16at(local + 26) + u16at(local + 28);
            let raw = &z[start..start + csize];
            let data = if method == 0 {
                raw.to_vec()
            } else {
                let mut o = Vec::new();
                DeflateDecoder::new(raw).read_to_end(&mut o).unwrap();
                o
            };
            assert_eq!(data.len(), usize_);
            assert_eq!(crc32(&data), u32at(off + 16) as u32);
            files.push((format!("/{name}"), make_file(data, "t", attr & 0o777)));
            off += 46 + nlen;
        }
        Tree::from_files(files).unwrap()
    }

    fn reversed(t: &Tree) -> Tree {
        let mut v: Vec<(String, Node)> = t.entries().map(|(p, n)| (p.clone(), n.clone())).collect();
        v.reverse();
        Tree::from_files(v).unwrap()
    }

    #[test]
    fn round_trips_and_determinism() {
        let o = WriterOptions::default();
        let t = sample();
        type W = fn(&Tree, &WriterOptions) -> Result<Vec<u8>>;
        let writers: [(&str, W); 3] = [("zip", write_zip), ("tar", write_tar), ("tgz", write_tgz)];
        for (name, w) in writers {
            let bytes = w(&t, &o).unwrap();
            let back = match name {
                "zip" => read_zip(&bytes),
                "tar" => read_tar(&bytes),
                _ => read_tar(&gunzip(&bytes)),
            };
            assert_eq!(back.digest(), t.digest(), "{name}");
            assert_eq!(back.get("/bin/run.sh").unwrap().mode, 0o755);
            assert_eq!(back.get("/index.html").unwrap().mode, 0o644);
            assert_eq!(bytes, w(&sample(), &o).unwrap(), "{name} deterministic");
            assert_eq!(bytes, w(&reversed(&t), &o).unwrap(), "{name} order independent");
            let empty = w(&Tree::empty(), &o).unwrap();
            assert!(!empty.is_empty());
        }
        assert_eq!(write_tar(&Tree::empty(), &o).unwrap(), vec![0u8; 1024]);
    }

    #[test]
    fn epoch_changes_timestamps() {
        let t = sample();
        let e = WriterOptions { epoch: Some(1_700_000_000), dirs: vec![] };
        let a = write_zip(&t, &e).unwrap();
        assert_eq!(a, write_zip(&t, &e).unwrap());
        assert_ne!(a, write_zip(&t, &WriterOptions::default()).unwrap());
        let tar = write_tar(&t, &e).unwrap();
        assert_eq!(u64::from_str_radix(std::str::from_utf8(&tar[136..147]).unwrap(), 8).unwrap(), 1_700_000_000);
        // 2023-11-14 22:13:20 UTC
        let (time, date) = dos_date_time(Some(1_700_000_000));
        assert_eq!(time, (22 << 11) | (13 << 5) | 10);
        assert_eq!(date, ((2023 - 1980) << 9) | (11 << 5) | 14);
        assert_eq!(dos_date_time(Some(5)), (0, (1 << 5) | 1));
        assert_eq!(source_date_epoch_from(Some("12")).unwrap(), Some(12));
        assert_eq!(source_date_epoch_from(Some("")).unwrap(), None);
        assert_eq!(source_date_epoch_from(Some("-1")).unwrap_err().code, E_USAGE);
        assert_eq!(source_date_epoch_from(Some("1234567890123")).unwrap_err().code, E_USAGE);
    }

    #[test]
    fn crc_and_iso() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(iso_millis(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_millis(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
        assert_eq!(iso_millis(951_782_400_000), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn zip_details() {
        let t = sample();
        let z = write_zip(&t, &WriterOptions::default()).unwrap();
        let u16at = |o: usize| u16::from_le_bytes([z[o], z[o + 1]]);
        let u32at = |o: usize| u32::from_le_bytes([z[o], z[o + 1], z[o + 2], z[o + 3]]);
        assert_eq!(u32at(0), 0x0403_4b50);
        assert_eq!(u16at(6) & 0x0800, 0x0800);
        assert_eq!(u16at(10), 0);
        assert_eq!(u16at(12), (1 << 5) | 1);
        let eocd = z.len() - 22;
        let count = u16at(eocd + 10) as usize;
        assert_eq!(count, t.len());
        let mut off = u32at(eocd + 16) as usize;
        let mut seen = std::collections::BTreeMap::new();
        let mut order = Vec::new();
        for _ in 0..count {
            let nlen = u16at(off + 28) as usize;
            let name = String::from_utf8(z[off + 46..off + 46 + nlen].to_vec()).unwrap();
            seen.insert(name.clone(), (u16at(off + 10), u32at(off + 38) >> 16, u16at(off + 4)));
            order.push(name);
            off += 46 + nlen;
        }
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(order, sorted);
        assert_eq!(seen["bin/run.sh"].1, 0o100755);
        assert_eq!(seen["index.html"].1, 0o100644);
        assert_eq!(seen["index.html"].2 >> 8, 3);
        assert_eq!(seen["index.html"].0, 8);
        assert_eq!(seen["empty.txt"].0, 0);
        assert!(seen.contains_key("ünï/cødé.txt"));
    }

    #[test]
    fn zip_entry_limit() {
        let files = (0..65536).map(|i| (format!("/f{i}"), make_file(Vec::new(), "t", 0o644)));
        let t = Tree::from_files(files).unwrap();
        assert_eq!(write_zip(&t, &WriterOptions::default()).unwrap_err().code, E_EXPORT);
    }

    #[test]
    fn tar_header_fields_and_checksum() {
        let tar = write_tar(&sample(), &WriterOptions::default()).unwrap();
        assert_eq!(&tar[257..263], b"ustar\0");
        let mut h = tar[0..512].to_vec();
        let stored = u64::from_str_radix(std::str::from_utf8(&h[148..154]).unwrap(), 8).unwrap();
        h[148..156].fill(0x20);
        assert_eq!(h.iter().map(|&b| b as u64).sum::<u64>(), stored);
        assert_eq!(tar.len() % 512, 0);
        assert_eq!(&tar[108..116], b"0000000\0");
        assert_eq!(&tar[265..297], &[0u8; 32]);
    }

    #[test]
    fn tar_long_names() {
        let seg = "a".repeat(50);
        let medium = format!("/{seg}/{seg}/{seg}/file.txt");
        let long = format!("/{}/deep.txt", [&seg; 6].map(|s| s.as_str()).join("/"));
        let one_big = format!("/{}.txt", "x".repeat(130));
        let t = mk(&[(&medium, b"m", 0o644), (&long, b"l", 0o644), (&one_big, b"o", 0o644), ("/short.txt", b"s", 0o644)]);
        let tar = write_tar(&t, &WriterOptions::default()).unwrap();
        let mut prefix_used = false;
        let mut pax = 0;
        let mut off = 0;
        while off + 512 <= tar.len() && tar[off..off + 512].iter().any(|&b| b != 0) {
            let h = &tar[off..off + 512];
            let size = u64::from_str_radix(std::str::from_utf8(&h[124..135]).unwrap(), 8).unwrap() as usize;
            if h[156] == b'x' {
                pax += 1;
            }
            if h[156] == b'0' && h[345] != 0 {
                prefix_used = true;
            }
            off += 512 + size.div_ceil(512) * 512;
        }
        assert!(prefix_used);
        assert!(pax >= 2);
        assert_eq!(read_tar(&tar).digest(), t.digest());
        let p = format!("/{}.txt", "é".repeat(120));
        let t2 = mk(&[(&p, b"x", 0o644)]);
        assert_eq!(read_tar(&write_tar(&t2, &WriterOptions::default()).unwrap()).digest(), t2.digest());
        assert_eq!(pax_record("path", "abc"), b"12 path=abc\n");
        let r = pax_record("path", &"x".repeat(93)); // body 100 -> length 103
        assert_eq!(&r[..4], b"103 ");
        assert_eq!(r.len(), 103);
    }

    #[test]
    fn tgz_header() {
        let t = sample();
        let gz = write_tgz(&t, &WriterOptions::default()).unwrap();
        assert_eq!(&gz[4..8], &[0, 0, 0, 0]);
        assert_eq!(gz[9], 3);
        assert_eq!(gunzip(&gz), write_tar(&t, &WriterOptions::default()).unwrap());
    }

    /// Byte-for-byte against the TypeScript writers (fixtures made with lib/src/archive-writers.js).
    #[test]
    fn identical_to_typescript_writers() {
        use crate::digest::sha256;
        let none = WriterOptions::default();
        assert_eq!(sha256(&write_tar(&sample(), &none).unwrap()), "sha256:1bcf5913cc11c1ed8a41df3076c060bf47dc03fab0b5328056e758160536b366");
        let e = WriterOptions { epoch: Some(1_700_000_000), dirs: vec![] };
        assert_eq!(sha256(&write_tar(&sample(), &e).unwrap()), "sha256:c988f1cd0314d1f58b3d6b9bf2b807ad95cc5659fb55fb9f413a9600b1dc76d0");
        let seg = "a".repeat(50);
        let longs = mk(&[
            (&format!("/{seg}/{seg}/{seg}/file.txt"), b"m", 0o644),
            (&format!("/{}/deep.txt", [&seg; 6].map(|s| s.as_str()).join("/")), b"l", 0o644),
            (&format!("/{}.txt", "x".repeat(130)), b"o", 0o644),
            (&format!("/{}.txt", "\u{e9}".repeat(120)), b"e", 0o644),
        ]);
        assert_eq!(sha256(&write_tar(&longs, &none).unwrap()), "sha256:7bb185b469e8790ee2a3864c98a91aaba891b1e418e1604aa05b3362f4174e57");
        // Entries that deflate cannot shrink are stored, so the whole zip is identical.
        let stored = mk(&[("/a.txt", b"a", 0o644), ("/b/c.bin", b"xyz", 0o755), ("/empty", b"", 0o644)]);
        assert_eq!(sha256(&write_zip(&stored, &none).unwrap()), "sha256:247ec13fd1565e2894ea61a9579047f5baaaa39ff1787c348d1159597db85bc5");
        let e1 = WriterOptions { epoch: Some(1_700_000_001), dirs: vec![] };
        assert_eq!(sha256(&write_zip(&stored, &e1).unwrap()), "sha256:6c780535188c6bea3bafc6499d49057f8e33a4140fa29d78d48805a0b6ae0738");
    }

    fn has_tool(cmd: &str, arg: &str) -> bool {
        std::process::Command::new(cmd).arg(arg).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().is_ok()
    }

    #[test]
    fn system_tools_accept_the_archives() {
        let dir = crate::export::exporter::tests::TmpDir::new("fmt");
        if has_tool("unzip", "-v") {
            let f = dir.path().join("a.zip");
            std::fs::write(&f, write_zip(&sample(), &WriterOptions::default()).unwrap()).unwrap();
            let out = std::process::Command::new("unzip").arg("-t").arg(&f).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
        }
        if has_tool("tar", "--version") {
            let seg = "a".repeat(50);
            let long = format!("/{}/deep.txt", [&seg; 6].map(|s| s.as_str()).join("/"));
            let t = mk(&[("/index.html", b"hi", 0o644), ("/bin/x.sh", b"#!/bin/sh\n", 0o755), (&long, b"d", 0o644)]);
            let gz = dir.path().join("a.tar.gz");
            std::fs::write(&gz, write_tgz(&t, &WriterOptions::default()).unwrap()).unwrap();
            let out = std::process::Command::new("tar").arg("-tzf").arg(&gz).output().unwrap();
            assert!(out.status.success());
            let listing = String::from_utf8_lossy(&out.stdout);
            assert!(listing.contains("index.html") && listing.contains("deep.txt"), "{listing}");
        }
    }
}

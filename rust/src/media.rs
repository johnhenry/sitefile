//! Template helpers that write HTML for files of the tree (port of `src/media.ts`, issue #105):
//! media types, image sizes read from file headers, URLs, subresource integrity, and the markup
//! each helper writes. `render.rs` parses `${file …}` and calls [`render_file`].

use crate::error::{ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::export::wbn::encode_uri_component;
use crate::paths::normalize_virtual;
use crate::render::escape_html;
use base64::Engine;
use sha2::{Digest, Sha384};

/// Helpers that produce a value (escaped like any substituted value).
pub const VALUE_HELPERS: [&str; 8] = ["url", "width", "height", "type", "size", "integrity", "datauri", "srcset"];
/// Helpers that write a tag (trusted markup: inserted as-is; every attribute value is escaped).
pub const TAG_HELPERS: [&str; 10] = ["img", "video", "audio", "source", "script", "stylesheet", "preload", "modulepreload", "pre", "svg"];

/// Every helper, value helpers first.
pub fn file_helpers() -> impl Iterator<Item = &'static str> {
    VALUE_HELPERS.into_iter().chain(TAG_HELPERS)
}

/// The largest file `datauri` inlines.
pub const DATAURI_MAX: usize = 32768;

const MIME: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("avif", "image/avif"),
    ("svg", "image/svg+xml"),
    ("ico", "image/x-icon"),
    ("mp4", "video/mp4"),
    ("m4v", "video/mp4"),
    ("webm", "video/webm"),
    ("ogv", "video/ogg"),
    ("mov", "video/quicktime"),
    ("mp3", "audio/mpeg"),
    ("m4a", "audio/mp4"),
    ("aac", "audio/aac"),
    ("oga", "audio/ogg"),
    ("ogg", "audio/ogg"),
    ("opus", "audio/opus"),
    ("wav", "audio/wav"),
    ("flac", "audio/flac"),
    ("css", "text/css"),
    ("js", "text/javascript"),
    ("mjs", "text/javascript"),
    ("json", "application/json"),
    ("html", "text/html"),
    ("htm", "text/html"),
    ("txt", "text/plain"),
    ("xml", "application/xml"),
    ("pdf", "application/pdf"),
    ("wasm", "application/wasm"),
    ("woff", "font/woff"),
    ("woff2", "font/woff2"),
    ("ttf", "font/ttf"),
    ("otf", "font/otf"),
];

/// The lowercase extension of a path's last segment (`""` if none; a leading dot is no extension).
pub fn extension_of(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    match base.rfind('.') {
        Some(d) if d > 0 => base[d + 1..].to_lowercase(),
        _ => String::new(),
    }
}

/// The media type of a path, by extension (`application/octet-stream` when unknown).
pub fn media_type(path: &str) -> &'static str {
    let ext = extension_of(path);
    MIME.iter().find(|(e, _)| *e == ext).map(|(_, m)| *m).unwrap_or("application/octet-stream")
}

/// Extensions the image helpers accept.
const IMAGE_EXTENSIONS: [&str; 7] = ["png", "jpg", "jpeg", "gif", "webp", "avif", "svg"];

pub fn is_image_path(path: &str) -> bool {
    IMAGE_EXTENSIONS.contains(&extension_of(path).as_str())
}

/// The URL of a tree path: each segment percent-encoded like `encodeURIComponent`.
pub fn file_url(path: &str) -> String {
    path.split('/').map(encode_uri_component).collect::<Vec<_>>().join("/")
}

/// `sha384-<base64>`, the subresource integrity of `bytes`.
pub fn integrity_of(bytes: &[u8]) -> String {
    format!("sha384-{}", base64::engine::general_purpose::STANDARD.encode(Sha384::digest(bytes)))
}

// ------------------------------------------------------------ image headers

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageSize {
    pub width: u64,
    pub height: u64,
}

/// Raised by the readers below; turned into one message per format.
struct Malformed;
type R<T> = std::result::Result<T, Malformed>;

fn need(b: &[u8], end: usize) -> R<()> {
    if end > b.len() {
        Err(Malformed)
    } else {
        Ok(())
    }
}
fn u16be(b: &[u8], i: usize) -> u64 {
    ((b[i] as u64) << 8) | b[i + 1] as u64
}
fn u16le(b: &[u8], i: usize) -> u64 {
    b[i] as u64 | ((b[i + 1] as u64) << 8)
}
fn u24le(b: &[u8], i: usize) -> u64 {
    b[i] as u64 | ((b[i + 1] as u64) << 8) | ((b[i + 2] as u64) << 16)
}
fn u32be(b: &[u8], i: usize) -> u64 {
    ((b[i] as u64) << 24) | ((b[i + 1] as u64) << 16) | ((b[i + 2] as u64) << 8) | b[i + 3] as u64
}
fn u32le(b: &[u8], i: usize) -> u64 {
    b[i] as u64 | ((b[i + 1] as u64) << 8) | ((b[i + 2] as u64) << 16) | ((b[i + 3] as u64) << 24)
}
fn at(b: &[u8], i: usize, s: &[u8]) -> bool {
    b.get(i..i + s.len()) == Some(s)
}

#[derive(Clone, Copy)]
enum Format {
    Png,
    Jpeg,
    Gif,
    WebP,
    Avif,
}

impl Format {
    fn name(self) -> &'static str {
        match self {
            Format::Png => "PNG",
            Format::Jpeg => "JPEG",
            Format::Gif => "GIF",
            Format::WebP => "WebP",
            Format::Avif => "AVIF",
        }
    }
}

fn sniff(b: &[u8]) -> Option<Format> {
    if b.len() >= 8 && b[0] == 0x89 && at(b, 1, b"PNG\r\n\x1a\n") {
        return Some(Format::Png);
    }
    if b.len() >= 3 && b[0] == 0xff && b[1] == 0xd8 && b[2] == 0xff {
        return Some(Format::Jpeg);
    }
    if at(b, 0, b"GIF87a") || at(b, 0, b"GIF89a") {
        return Some(Format::Gif);
    }
    if at(b, 0, b"RIFF") && at(b, 8, b"WEBP") {
        return Some(Format::WebP);
    }
    if at(b, 4, b"ftyp") && is_avif_brand(b) {
        return Some(Format::Avif);
    }
    None
}

fn png_size(b: &[u8]) -> R<ImageSize> {
    need(b, 24)?;
    if &b[12..16] != b"IHDR" {
        return Err(Malformed);
    }
    Ok(ImageSize { width: u32be(b, 16), height: u32be(b, 20) })
}

fn gif_size(b: &[u8]) -> R<ImageSize> {
    need(b, 10)?;
    Ok(ImageSize { width: u16le(b, 6), height: u16le(b, 8) })
}

/// JPEG: the first SOF segment; EXIF orientation 5 to 8 (a quarter turn) swaps width and height.
fn jpeg_size(b: &[u8]) -> R<ImageSize> {
    let mut i = 2usize;
    let mut turned = false;
    loop {
        need(b, i + 2)?;
        if b[i] != 0xff {
            return Err(Malformed);
        }
        let mut m = b[i + 1];
        while m == 0xff {
            i += 1;
            need(b, i + 2)?;
            m = b[i + 1];
        }
        if m == 0x01 || (0xd0..=0xd8).contains(&m) {
            i += 2;
            continue;
        }
        if m == 0xd9 || m == 0xda {
            return Err(Malformed); // end of image or start of scan before any SOF
        }
        need(b, i + 4)?;
        let len = u16be(b, i + 2) as usize;
        if len < 2 {
            return Err(Malformed);
        }
        let end = i + 2 + len;
        need(b, end)?;
        let sof = matches!(m, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf);
        if sof {
            if len < 7 {
                return Err(Malformed);
            }
            let (height, width) = (u16be(b, i + 5), u16be(b, i + 7));
            return Ok(if turned { ImageSize { width: height, height: width } } else { ImageSize { width, height } });
        }
        if m == 0xe1 && !turned {
            turned = exif_quarter_turn(&b[i + 4..end]);
        }
        i = end;
    }
}

/// An APP1 segment's EXIF orientation is 5, 6, 7 or 8. Anything unreadable counts as no orientation.
fn exif_quarter_turn(s: &[u8]) -> bool {
    if s.len() < 14 || &s[..6] != b"Exif\0\0" {
        return false;
    }
    let t = &s[6..];
    let le = t[0] == 0x49 && t[1] == 0x49;
    if !le && !(t[0] == 0x4d && t[1] == 0x4d) {
        return false;
    }
    let u16 = |i: usize| if le { u16le(t, i) } else { u16be(t, i) };
    let u32 = |i: usize| if le { u32le(t, i) } else { u32be(t, i) };
    if u16(2) != 42 {
        return false;
    }
    let ifd = u32(4);
    if ifd + 2 > t.len() as u64 {
        return false;
    }
    let ifd = ifd as usize;
    let n = u16(ifd) as usize;
    for k in 0..n {
        let e = ifd + 2 + 12 * k;
        if e + 12 > t.len() {
            return false;
        }
        if u16(e) == 0x0112 {
            let v = if u16(e + 2) == 3 { u16(e + 8) } else { 0 };
            return (5..=8).contains(&v);
        }
    }
    false
}

fn webp_size(b: &[u8]) -> R<ImageSize> {
    need(b, 16)?;
    match &b[12..16] {
        b"VP8 " => {
            need(b, 30)?;
            if b[23] != 0x9d || b[24] != 0x01 || b[25] != 0x2a {
                return Err(Malformed);
            }
            Ok(ImageSize { width: u16le(b, 26) & 0x3fff, height: u16le(b, 28) & 0x3fff })
        }
        b"VP8L" => {
            need(b, 25)?;
            if b[20] != 0x2f {
                return Err(Malformed);
            }
            let bits = u32le(b, 21);
            Ok(ImageSize { width: (bits & 0x3fff) + 1, height: ((bits >> 14) & 0x3fff) + 1 })
        }
        b"VP8X" => {
            need(b, 30)?;
            Ok(ImageSize { width: u24le(b, 24) + 1, height: u24le(b, 27) + 1 })
        }
        _ => Err(Malformed),
    }
}

// AVIF: ISO-BMFF boxes. The size is the primary item's `ispe` property; `irot` 1 or 3 swaps it.

struct BoxRef {
    kind: [u8; 4],
    /// Start of the box's payload.
    start: usize,
    end: usize,
}

/// The boxes in b[from, to).
fn boxes(b: &[u8], from: usize, to: usize) -> R<Vec<BoxRef>> {
    let mut out = Vec::new();
    let mut i = from;
    while i < to {
        if i + 8 > to {
            return Err(Malformed);
        }
        let mut size = u32be(b, i) as usize;
        let kind = [b[i + 4], b[i + 5], b[i + 6], b[i + 7]];
        let mut start = i + 8;
        if size == 1 {
            if i + 16 > to || u32be(b, i + 8) != 0 {
                return Err(Malformed); // a 64-bit size over 4 GiB
            }
            size = u32be(b, i + 12) as usize;
            start = i + 16;
        } else if size == 0 {
            size = to - i;
        }
        if size < start - i || i + size > to {
            return Err(Malformed);
        }
        out.push(BoxRef { kind, start, end: i + size });
        i += size;
    }
    Ok(out)
}

fn is_avif_brand(b: &[u8]) -> bool {
    if b.len() < 16 {
        return false;
    }
    let end = (u32be(b, 0) as usize).min(b.len());
    let mut i = 8;
    while i + 4 <= end {
        if i != 12 && (&b[i..i + 4] == b"avif" || &b[i..i + 4] == b"avis") {
            return true;
        }
        i += 4;
    }
    false
}

fn avif_size(b: &[u8]) -> R<ImageSize> {
    let top = boxes(b, 0, b.len())?;
    let meta = top.iter().find(|x| &x.kind == b"meta").ok_or(Malformed)?;
    if meta.start + 4 > meta.end {
        return Err(Malformed);
    }
    let inner = boxes(b, meta.start + 4, meta.end)?; // a full box: version and flags first
    let pitm = inner.iter().find(|x| &x.kind == b"pitm").ok_or(Malformed)?;
    let iprp = inner.iter().find(|x| &x.kind == b"iprp").ok_or(Malformed)?;
    if pitm.start + 6 > pitm.end {
        return Err(Malformed);
    }
    let primary = if b[pitm.start] == 0 {
        u16be(b, pitm.start + 4)
    } else {
        need(b, pitm.start + 8)?;
        u32be(b, pitm.start + 4)
    };
    let props = boxes(b, iprp.start, iprp.end)?;
    let ipco = props.iter().find(|x| &x.kind == b"ipco").ok_or(Malformed)?;
    let ipma = props.iter().find(|x| &x.kind == b"ipma").ok_or(Malformed)?;
    let properties = boxes(b, ipco.start, ipco.end)?;
    need(b, ipma.start + 8)?;
    let version = b[ipma.start];
    let wide = b[ipma.start + 3] & 1 == 1;
    let mut i = ipma.start + 4;
    let entries = u32be(b, i);
    i += 4;
    let mut associated: Option<Vec<usize>> = None;
    for _ in 0..entries {
        let id_len = if version < 1 { 2 } else { 4 };
        if i + id_len + 1 > ipma.end {
            return Err(Malformed);
        }
        let id = if id_len == 2 { u16be(b, i) } else { u32be(b, i) };
        i += id_len;
        let count = b[i];
        i += 1;
        let mut list = Vec::new();
        for _ in 0..count {
            if wide {
                if i + 2 > ipma.end {
                    return Err(Malformed);
                }
                list.push((u16be(b, i) & 0x7fff) as usize);
                i += 2;
            } else {
                if i + 1 > ipma.end {
                    return Err(Malformed);
                }
                list.push((b[i] & 0x7f) as usize);
                i += 1;
            }
        }
        if id == primary && associated.is_none() {
            associated = Some(list);
        }
    }
    let associated = associated.ok_or(Malformed)?;
    let mut size: Option<ImageSize> = None;
    let mut turned = false;
    for index in associated {
        let Some(p) = index.checked_sub(1).and_then(|k| properties.get(k)) else { continue };
        if &p.kind == b"ispe" && size.is_none() {
            if p.start + 12 > p.end {
                return Err(Malformed);
            }
            size = Some(ImageSize { width: u32be(b, p.start + 4), height: u32be(b, p.start + 8) });
        } else if &p.kind == b"irot" && p.start < p.end {
            turned = (b[p.start] & 3) % 2 == 1;
        }
    }
    let size = size.ok_or(Malformed)?;
    Ok(if turned { ImageSize { width: size.height, height: size.width } } else { size })
}

/// The width and height of an image file, from its header (PNG, JPEG, GIF, WebP, AVIF): bounded
/// reads, no decoding. `None` for an SVG. E_DATA (without a span) when the bytes are no supported
/// image, or the header is truncated or malformed.
pub fn image_size(bytes: &[u8], path: &str) -> Result<Option<ImageSize>> {
    if extension_of(path) == "svg" {
        return Ok(None);
    }
    let Some(format) = sniff(bytes) else {
        return Err(SitefileError::plain(E_DATA, format!("'{path}' is not a PNG, JPEG, GIF, WebP or AVIF image")));
    };
    let size = match format {
        Format::Png => png_size(bytes),
        Format::Jpeg => jpeg_size(bytes),
        Format::Gif => gif_size(bytes),
        Format::WebP => webp_size(bytes),
        Format::Avif => avif_size(bytes),
    };
    match size {
        Ok(s) if s.width != 0 && s.height != 0 => Ok(Some(s)),
        _ => Err(SitefileError::plain(E_DATA, format!("'{path}' is not a valid {} image: its header is truncated or malformed", format.name()))),
    }
}

// ------------------------------------------------------------ helpers

/// A file of the tree a helper writes markup for.
pub struct TreeFile<'a> {
    /// Its normalized absolute tree path.
    pub path: String,
    pub bytes: &'a [u8],
}

/// An attribute written in the template: `alt="…"` (value) or `controls` (bare).
#[derive(Debug, Clone, PartialEq)]
pub struct HelperAttr {
    pub name: String,
    pub value: Option<String>,
}

/// What a helper produced: markup (inserted as-is) or a value (escaped like any `${…}`).
pub enum HelperOutput {
    Markup(String),
    Value(String),
}

/// The files of the stage's tree at a RENDER, as `${file …}` sees them.
pub trait TemplateTree {
    fn get(&self, path: &str) -> Option<&[u8]>;
    fn is_directory(&self, path: &str) -> bool;
    /// Where a FINGERPRINT earlier in the stage moved `path` (its content-hashed name), if it did.
    fn renamed(&self, _path: &str) -> Option<String> {
        None
    }
}

/// RENDER options the helpers read.
#[derive(Debug, Clone, Copy, Default)]
pub struct HelperOptions {
    /// RENDER --inline-svg: the svg helper may paste SVG markup into the page.
    pub inline_svg: bool,
}

fn decode_text(f: &TreeFile, helper: &str, span: &SourceSpan) -> Result<String> {
    match std::str::from_utf8(f.bytes) {
        // TextDecoder strips a leading BOM.
        Ok(s) => Ok(s.strip_prefix('\u{feff}').unwrap_or(s).to_string()),
        Err(_) => Err(SitefileError::at(E_ENCODING, format!("'{}' is not valid UTF-8 (the {helper} helper writes its text into the page)", f.path), span)),
    }
}

/// `<name` and its attributes: the helper's own (`auto`, in order), then the template's, then the
/// helper's defaults. A template attribute named like an automatic or default one (ASCII
/// case-insensitive) replaces its value in place. Every value is HTML-escaped.
fn tag(name: &str, auto: &[(&str, Option<String>)], attrs: &[HelperAttr], defaults: &[(&str, &str)]) -> String {
    let given = |n: &str| attrs.iter().rev().find(|a| a.name.to_ascii_lowercase() == n);
    let own = |n: &str| auto.iter().any(|(a, _)| *a == n) || defaults.iter().any(|(d, _)| *d == n);
    let one = |n: &str, v: Option<&str>| match v {
        None => format!(" {n}"),
        Some(v) => format!(" {n}=\"{}\"", escape_html(v)),
    };
    let pick = |n: &str, v: Option<&str>| match given(n) {
        Some(g) => one(&g.name, g.value.as_deref()),
        None => one(n, v),
    };
    let mut out = format!("<{name}");
    for (n, v) in auto {
        out.push_str(&pick(n, v.as_deref()));
    }
    for a in attrs {
        if !own(&a.name.to_ascii_lowercase()) {
            out.push_str(&one(&a.name, a.value.as_deref()));
        }
    }
    for (n, v) in defaults {
        out.push_str(&pick(n, Some(v)));
    }
    out
}

/// `as` of a preload, from the extension.
fn preload_as(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "css" => "style",
        "js" | "mjs" => "script",
        "woff" | "woff2" | "ttf" | "otf" => "font",
        "json" => "fetch",
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "svg" => "image",
        _ => return None,
    })
}

/// The file `${file "<raw>"}` names: an absolute path in the tree (E_PATH, E_TYPE, E_NOT_FOUND otherwise).
pub fn resolve_file<'t>(raw: &str, tree: &'t dyn TemplateTree, span: &SourceSpan) -> Result<TreeFile<'t>> {
    let p = normalize_virtual(raw, Some(span), "${file} path")?;
    if p.path == "/" || tree.is_directory(&p.path) {
        return Err(SitefileError::at(E_TYPE, format!("'{}' is a directory; ${{file}} needs a file", p.path), span));
    }
    let mut path = p.path;
    let mut found = tree.get(&path);
    // Renamed by FINGERPRINT: the hashed file, so the URL and integrity are those of what is served.
    if found.is_none() {
        if let Some(moved) = tree.renamed(&path) {
            if let Some(b) = tree.get(&moved) {
                path = moved;
                found = Some(b);
            }
        }
    }
    let Some(bytes) = found else {
        return Err(SitefileError::at(
            E_NOT_FOUND,
            format!("'{}' is not in the tree at this RENDER (write it with COPY, RENDER or TRANSFORM on an earlier line)", path),
            span,
        ));
    };
    Ok(TreeFile { path: path, bytes })
}

/// What `${file "<raw>" | helper attrs…}` writes (`helper` None: the file's URL).
pub fn render_file(raw: &str, helper: Option<&str>, attrs: &[HelperAttr], tree: &dyn TemplateTree, opts: HelperOptions, span: &SourceSpan) -> Result<HelperOutput> {
    // The image plugin's metadata, when there is some, is what the image helpers read (#128).
    if helper.is_none_or(|h| METADATA_HELPERS.contains(&h)) {
        let path = normalize_virtual(raw, Some(span), "${file} path")?.path;
        if let Some(meta) = find_metadata(&path, helper, tree, span)? {
            return Ok(render_metadata(helper, &meta, attrs));
        }
    }
    let f = resolve_file(raw, tree, span)?;
    render_file_helper(helper, &f, attrs, opts, span)
}

/// What `helper` writes for file `f` (`None`: no helper, the file's URL).
pub fn render_file_helper(helper: Option<&str>, f: &TreeFile, attrs: &[HelperAttr], opts: HelperOptions, span: &SourceSpan) -> Result<HelperOutput> {
    use HelperOutput::{Markup, Value};
    let url = file_url(&f.path);
    let ext = extension_of(&f.path);
    let image = |what: &str| -> Result<Option<ImageSize>> {
        if !IMAGE_EXTENSIONS.contains(&ext.as_str()) {
            return Err(SitefileError::at(
                E_TYPE,
                format!("'{}' is not an image ({what} reads .png, .jpg, .jpeg, .gif, .webp, .avif and .svg files)", f.path),
                span,
            ));
        }
        image_size(f.bytes, &f.path).map_err(|e| if e.span.is_none() { SitefileError::at(e.code, e.detail, span) } else { e })
    };
    let integrity = || integrity_of(f.bytes);
    Ok(match helper {
        None | Some("url") => Value(url),
        Some("type") => Value(media_type(&f.path).to_string()),
        Some("size") => Value(f.bytes.len().to_string()),
        Some("integrity") => Value(integrity()),
        Some(h @ ("width" | "height")) => {
            let Some(size) = image(h)? else {
                return Err(SitefileError::at(E_TYPE, format!("'{}' is an SVG image; its {h} is in its markup, not in a header", f.path), span));
            };
            Value(if h == "width" { size.width } else { size.height }.to_string())
        }
        Some("srcset") => {
            return Err(SitefileError::at(
                E_TYPE,
                format!("'{}' has no image metadata: srcset reads the <name>.json the image plugin writes (TRANSFORM … WITH image TO /dir/)", f.path),
                span,
            ));
        }
        Some("datauri") => {
            if f.bytes.len() > DATAURI_MAX {
                return Err(SitefileError::at(
                    E_TOO_LARGE,
                    format!("'{}' is {} bytes; datauri inlines files of at most {DATAURI_MAX} bytes (link to it instead)", f.path, f.bytes.len()),
                    span,
                ));
            }
            Value(format!("data:{};base64,{}", media_type(&f.path), base64::engine::general_purpose::STANDARD.encode(f.bytes)))
        }
        Some("img") => {
            let size = image("img")?;
            let mut auto = vec![("src", Some(url))];
            if let Some(s) = size {
                auto.push(("width", Some(s.width.to_string())));
                auto.push(("height", Some(s.height.to_string())));
            }
            Markup(tag("img", &auto, attrs, &[("loading", "lazy"), ("decoding", "async")]) + ">")
        }
        Some(h @ ("video" | "audio")) => Markup(format!("{}></{h}>", tag(h, &[("src", Some(url))], attrs, &[]))),
        Some("source") => {
            let key = if is_image_path(&f.path) { "srcset" } else { "src" };
            Markup(tag("source", &[(key, Some(url)), ("type", Some(media_type(&f.path).to_string()))], attrs, &[]) + ">")
        }
        Some("script") => Markup(tag("script", &[("src", Some(url)), ("integrity", Some(integrity()))], attrs, &[]) + "></script>"),
        Some("stylesheet") => Markup(tag("link", &[("rel", Some("stylesheet".into())), ("href", Some(url)), ("integrity", Some(integrity()))], attrs, &[]) + ">"),
        Some("modulepreload") => {
            Markup(tag("link", &[("rel", Some("modulepreload".into())), ("href", Some(url)), ("integrity", Some(integrity()))], attrs, &[]) + ">")
        }
        Some("preload") => {
            let given = attrs.iter().find(|a| a.name.to_ascii_lowercase() == "as").and_then(|a| a.value.clone());
            let Some(as_) = given.or_else(|| preload_as(&ext).map(str::to_string)) else {
                return Err(SitefileError::at(
                    E_TYPE,
                    format!("preload cannot tell what '{}' is; give it as=… (style, script, font, image, fetch, …)", f.path),
                    span,
                ));
            };
            let mut auto = vec![("rel", Some("preload".to_string())), ("href", Some(url)), ("as", Some(as_.clone()))];
            // Browsers check integrity on preloaded scripts and stylesheets; fonts and fetches load in CORS mode.
            if as_ == "script" || as_ == "style" {
                auto.push(("integrity", Some(integrity())));
            }
            if as_ == "font" || as_ == "fetch" {
                auto.push(("crossorigin", None));
            }
            Markup(tag("link", &auto, attrs, &[]) + ">")
        }
        Some("pre") => Markup(format!("{}>{}</pre>", tag("pre", &[], attrs, &[]), escape_html(&decode_text(f, "pre", span)?))),
        Some("svg") => {
            if !opts.inline_svg {
                return Err(SitefileError::at(
                    E_UNSUPPORTED,
                    format!(
                        "the svg helper pastes '{}' into the page, and SVG markup can run scripts; allow it with RENDER --inline-svg, or use the img helper",
                        f.path
                    ),
                    span,
                ));
            }
            let text = strip_svg_prolog(&decode_text(f, "svg", span)?);
            let ok = text.starts_with("<svg") && text.as_bytes().get(4).is_some_and(|c| b"\t\n\x0c\r />".contains(c));
            if !ok {
                return Err(SitefileError::at(E_DATA, format!("'{}' does not start with an <svg> element", f.path), span));
            }
            Markup(tag("svg", &[], attrs, &[]) + &text[4..])
        }
        Some(other) => return Err(SitefileError::at(E_TEMPLATE, format!("unknown file helper '{other}'"), span)),
    })
}

fn is_svg_ws(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\x0c' | '\r' | ' ')
}

/// SVG text without a BOM, XML declaration, comments before the root, doctype and surrounding whitespace.
fn strip_svg_prolog(s: &str) -> String {
    let mut t = s.strip_prefix('\u{feff}').unwrap_or(s).trim_start_matches(is_svg_ws);
    loop {
        let end = if t.starts_with("<?xml") {
            t.find("?>").map(|k| k + 2)
        } else if t.starts_with("<!--") {
            t.find("-->").map(|k| k + 3)
        } else if t.len() >= 9 && t.is_char_boundary(9) && t[..9].eq_ignore_ascii_case("<!DOCTYPE") {
            t.find('>').map(|k| k + 1)
        } else {
            None
        };
        match end {
            Some(e) => t = t[e..].trim_start_matches(is_svg_ws),
            None => break,
        }
    }
    t.trim_end_matches(is_svg_ws).to_string()
}

// ------------------------------------------------------------ image plugin metadata (#128)

/// The image plugin's `<name>.json` (docs/PLUGINS.md, "The metadata file"), version 1: what the helpers read.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageMetadata {
    /// The metadata file's tree path (for messages).
    pub path: String,
    pub src: String,
    pub kind: String,
    pub width: u64,
    pub height: u64,
    pub srcset: String,
    /// (type, srcset) per format, fallback last.
    pub sources: Vec<(String, String)>,
}

/// The metadata version this sitefile reads.
pub const IMAGE_METADATA_VERSION: u64 = 1;

/// Helpers that read image metadata when there is some.
const METADATA_HELPERS: [&str; 6] = ["url", "type", "width", "height", "srcset", "img"];

/// Looks like the plugin's metadata (an object with a number `version` and a `variants` list).
fn looks_like_metadata(v: &serde_json::Value) -> bool {
    v.get("version").is_some_and(|x| x.is_number()) && v.get("variants").is_some_and(|x| x.is_array()) && v.is_object()
}

fn parse_json(bytes: &[u8]) -> Option<serde_json::Value> {
    let s = std::str::from_utf8(bytes).ok()?;
    serde_json::from_str(s.strip_prefix('\u{feff}').unwrap_or(s)).ok()
}

fn js_number(x: &serde_json::Value) -> String {
    match x.as_f64() {
        Some(f) => crate::values::Scalar::Num(f).to_js_string(),
        None => x.to_string(),
    }
}

fn read_metadata(v: &serde_json::Value, path: &str, span: &SourceSpan) -> Result<ImageMetadata> {
    let version = &v["version"];
    if version.as_f64() != Some(IMAGE_METADATA_VERSION as f64) {
        return Err(SitefileError::at(
            E_DATA,
            format!("'{path}' is image metadata version {}; this sitefile reads version {IMAGE_METADATA_VERSION} (upgrade sitefile)", js_number(version)),
            span,
        ));
    }
    let bad = |field: &str, what: &str| SitefileError::at(E_DATA, format!("'{path}' is not valid image metadata: '{field}' must be {what}"), span);
    let string = |o: &serde_json::Value, k: &str, field: &str| -> Result<String> { o.get(k).and_then(|x| x.as_str()).map(str::to_string).ok_or_else(|| bad(field, "a string")) };
    let dim = |k: &str| -> Result<u64> {
        match v.get(k).and_then(|x| x.as_f64()) {
            Some(f) if f > 0.0 && f.fract() == 0.0 && f <= 9007199254740991.0 => Ok(f as u64),
            _ => Err(bad(k, "a positive whole number")),
        }
    };
    let src = string(v, "src", "src")?;
    let kind = string(v, "type", "type")?;
    let width = dim("width")?;
    let height = dim("height")?;
    let srcset = string(v, "srcset", "srcset")?;
    let Some(list) = v.get("sources").and_then(|x| x.as_array()) else { return Err(bad("sources", "a list")) };
    let mut sources = Vec::new();
    for (i, s) in list.iter().enumerate() {
        if !s.is_object() {
            return Err(bad(&format!("sources.{i}"), "an object"));
        }
        sources.push((string(s, "type", &format!("sources.{i}.type"))?, string(s, "srcset", &format!("sources.{i}.srcset"))?));
    }
    Ok(ImageMetadata { path: path.to_string(), src, kind, width, height, srcset, sources })
}

/// `/dir/name.ext` -> `/dir/name.json` (`""` for a name without an extension).
pub fn metadata_path_of(path: &str) -> String {
    let slash = path.rfind('/').map(|k| k as i64).unwrap_or(-1);
    match path.rfind('.') {
        Some(dot) if dot as i64 > slash + 1 => format!("{}.json", &path[..dot]),
        _ => String::new(),
    }
}

/// The image metadata a helper reads for `path`, if any (see src/media.ts findMetadata).
fn find_metadata(path: &str, helper: Option<&str>, tree: &dyn TemplateTree, span: &SourceSpan) -> Result<Option<ImageMetadata>> {
    let ext = extension_of(path);
    if ext == "json" {
        if !helper.is_some_and(|h| ["width", "height", "srcset", "img"].contains(&h)) {
            return Ok(None);
        }
        let Some(bytes) = located(path, tree) else { return Ok(None) };
        let v = parse_json(bytes).filter(looks_like_metadata).ok_or_else(|| {
            SitefileError::at(E_DATA, format!("'{path}' is not image metadata (the <name>.json the image plugin writes next to the files it makes)"), span)
        })?;
        return read_metadata(&v, path, span).map(|m| Some(moved(m, tree)));
    }
    if !IMAGE_EXTENSIONS.contains(&ext.as_str()) {
        return Ok(None);
    }
    let m = metadata_path_of(path);
    let found = if m.is_empty() { None } else { located(&m, tree) };
    let Some(bytes) = found else { return Ok(None) };
    match parse_json(bytes) {
        Some(v) if looks_like_metadata(&v) => read_metadata(&v, &m, span).map(|m| Some(moved(m, tree))),
        _ => Ok(None),
    }
}

/// A tree path's bytes, or those of where a FINGERPRINT moved it.
fn located<'t>(path: &str, tree: &'t dyn TemplateTree) -> Option<&'t [u8]> {
    tree.get(path).or_else(|| tree.renamed(path).and_then(|to| tree.get(&to)))
}

/// Metadata read after a FINGERPRINT: a variant it renamed gets the hashed file's URL (see src/media.ts moved).
fn moved(m: ImageMetadata, tree: &dyn TemplateTree) -> ImageMetadata {
    const WS: &[char] = &[' ', '\t', '\n', '\r', '\x0c'];
    let srcset = |set: &str| -> String {
        set.split(',')
            .map(|c| {
                let t = c.trim_start_matches(WS);
                let u = t.find(WS).map_or(t, |end| &t[..end]);
                let to = moved_url(u, tree);
                if to == u { c.to_string() } else { c.replacen(u, &to, 1) }
            })
            .collect::<Vec<_>>()
            .join(",")
    };
    ImageMetadata {
        src: moved_url(&m.src, tree),
        srcset: srcset(&m.srcset),
        sources: m.sources.iter().map(|(t, s)| (t.clone(), srcset(s))).collect(),
        ..m
    }
}

/// An absolute URL path FINGERPRINT renamed: the hashed file's URL; any other URL as it is.
fn moved_url(url: &str, tree: &dyn TemplateTree) -> String {
    if !url.starts_with('/') {
        return url.to_string();
    }
    let path = url.split('/').map(crate::links::percent_decode).collect::<Vec<_>>().join("/");
    match tree.renamed(&path) {
        Some(to) if tree.get(&to).is_some() => file_url(&to),
        _ => url.to_string(),
    }
}

fn render_metadata(helper: Option<&str>, m: &ImageMetadata, attrs: &[HelperAttr]) -> HelperOutput {
    use HelperOutput::{Markup, Value};
    match helper {
        None | Some("url") => Value(m.src.clone()),
        Some("type") => Value(m.kind.clone()),
        Some("width") => Value(m.width.to_string()),
        Some("height") => Value(m.height.to_string()),
        Some("srcset") => Value(m.srcset.clone()),
        _ => {
            let auto = [("src", Some(m.src.clone())), ("srcset", Some(m.srcset.clone())), ("width", Some(m.width.to_string())), ("height", Some(m.height.to_string()))];
            let img = tag("img", &auto, attrs, &[("loading", "lazy"), ("decoding", "async")]) + ">";
            if m.sources.len() <= 1 {
                return Markup(img);
            }
            // `sizes` applies to every <source> of the picture too.
            let sizes = attrs.iter().find(|a| a.name.to_ascii_lowercase() == "sizes");
            let mut out = String::from("<picture>");
            for (t, s) in &m.sources[..m.sources.len() - 1] {
                let mut auto = vec![("type", Some(t.clone())), ("srcset", Some(s.clone()))];
                if let Some(z) = sizes {
                    auto.push(("sizes", z.value.clone()));
                }
                out.push_str(&(tag("source", &auto, &[], &[]) + ">"));
            }
            Markup(format!("{out}{img}</picture>"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_types_urls() {
        assert_eq!(extension_of("/a/b.JPG"), "jpg");
        assert_eq!(extension_of("/a/.hidden"), "");
        assert_eq!(media_type("/x.woff2"), "font/woff2");
        assert_eq!(media_type("/x"), "application/octet-stream");
        assert_eq!(file_url("/img/my photo é.png"), "/img/my%20photo%20%C3%A9.png");
        assert_eq!(integrity_of(b"h1 { color: teal }\n"), "sha384-sqPOxExJseEdur+S1v+wqJueVTMC7/XLWGSg0TFlWxyyxuVLvELs2g757ehC4aUg");
    }

    /// `tests/fixtures/media/fuzz.json` (generated from src/media.ts): the same size or the same error.
    #[test]
    fn differential_fixtures() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/media/fuzz.json");
        let cases: Vec<serde_json::Value> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert!(cases.len() >= 1000, "only {} cases", cases.len());
        let mut bad = Vec::new();
        for c in &cases {
            let name = c["name"].as_str().unwrap();
            let bytes = base64::engine::general_purpose::STANDARD.decode(c["bytes"].as_str().unwrap()).unwrap();
            let got = match image_size(&bytes, &format!("/{name}")) {
                Ok(Some(s)) => serde_json::json!({ "ok": [s.width, s.height] }),
                Ok(None) => serde_json::json!({ "ok": null }),
                Err(e) => serde_json::json!({ "error": [e.code.as_str(), e.detail] }),
            };
            let want = match c.get("error") {
                Some(e) => serde_json::json!({ "error": e }),
                None => serde_json::json!({ "ok": c["ok"] }),
            };
            if got != want {
                bad.push(format!("{name} {}: want {want} got {got}", c["bytes"]));
            }
        }
        assert!(bad.is_empty(), "{} of {} differ:\n{}", bad.len(), cases.len(), bad.iter().take(8).cloned().collect::<Vec<_>>().join("\n"));
    }

    #[test]
    fn strips_the_svg_prolog() {
        assert_eq!(strip_svg_prolog("\u{feff} <?xml version=\"1.0\"?>\n<!-- c -->\n<!doctype svg>\n<svg/>\n"), "<svg/>");
    }
}

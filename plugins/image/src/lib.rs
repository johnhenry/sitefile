//! Sitefile's first-party image plugin (WebAssembly, ABI v1).
//!
//! Decodes PNG, JPEG, GIF (first frame) and WebP (lossy or lossless, first
//! frame), resizes with a fixed Lanczos-3 filter (see `resize.rs`) and
//! encodes PNG, JPEG (baseline, fixed quality) or lossless WebP. Metadata
//! (EXIF, ICC profiles, text chunks) is not copied; EXIF orientation is
//! applied first. See docs/PLUGINS.md ("The image plugin") for the
//! options and the metadata file.
//!
//! The module imports nothing, so it can't touch files, the network or the
//! clock: the same request always gives the same bytes, under V8 and wasmtime
//! alike.

pub mod b64;
mod json;
pub mod resize;

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::codecs::webp::WebPEncoder;
use image::{DynamicImage, ExtendedColorType, ImageDecoder, ImageEncoder, ImageFormat, ImageReader, Limits};
use json::{quote, Json};
use std::alloc::{alloc, Layout};
use std::io::Cursor;

/// Largest decoded image, in pixels (width × height, after decoding the header).
pub const MAX_PIXELS: u64 = 40_000_000;
/// Largest output width.
pub const MAX_WIDTH: u32 = 16_384;
/// Most widths one TRANSFORM may ask for.
pub const MAX_WIDTHS: usize = 16;
/// Memory the decoders may allocate (the host caps the whole module at 1 GiB).
const MAX_ALLOC: u64 = 512 * 1024 * 1024;
const DEFAULT_QUALITY: u8 = 82;

const OPTIONS: &[&str] = &["widths", "format", "quality", "name", "max_pixels", "background"];

#[no_mangle]
pub extern "C" fn sitefile_alloc(len: i32) -> i32 {
    let layout = Layout::from_size_align(len.max(1) as usize, 1).unwrap();
    unsafe { alloc(layout) as i32 }
}

#[no_mangle]
pub extern "C" fn sitefile_api_version() -> i32 {
    1
}

#[no_mangle]
pub extern "C" fn sitefile_transform(ptr: i32, len: i32) -> i64 {
    let request = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let out = match transform(request) {
        Ok(Output::Bytes(b)) => format!("{{\"bytes\":\"{}\"}}", b64::encode(&b)),
        Ok(Output::Files(files)) => {
            let body: Vec<String> = files.iter().map(|(k, v)| format!("{}:\"{}\"", quote(k), b64::encode(v))).collect();
            format!("{{\"files\":{{{}}}}}", body.join(","))
        }
        Err(e) => format!("{{\"error\":{}}}", quote(&e)),
    };
    // Leak the result: the instance is thrown away after this call.
    let bytes = out.into_bytes().into_boxed_slice();
    let len = bytes.len() as i64;
    let ptr = Box::into_raw(bytes) as *mut u8 as i64;
    (ptr << 32) | len
}

pub enum Output {
    Bytes(Vec<u8>),
    /// Relative path → bytes, in a fixed order.
    Files(Vec<(String, Vec<u8>)>),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Png,
    Jpeg,
    Webp,
}

impl Format {
    fn parse(s: &str) -> Option<Format> {
        match s {
            "png" => Some(Format::Png),
            "jpeg" | "jpg" => Some(Format::Jpeg),
            "webp" => Some(Format::Webp),
            _ => None,
        }
    }
    fn ext(self) -> &'static str {
        match self {
            Format::Png => "png",
            Format::Jpeg => "jpg",
            Format::Webp => "webp",
        }
    }
    fn name(self) -> &'static str {
        match self {
            Format::Png => "png",
            Format::Jpeg => "jpeg",
            Format::Webp => "webp",
        }
    }
    fn mime(self) -> &'static str {
        match self {
            Format::Png => "image/png",
            Format::Jpeg => "image/jpeg",
            Format::Webp => "image/webp",
        }
    }
}

pub struct Decoded {
    pub format: &'static str,
    pub width: u32,
    pub height: u32,
    /// RGBA8, row-major.
    pub rgba: Vec<u8>,
}

/// Decode an image after checking its header against `max_pixels`.
pub fn decode(bytes: &[u8], max_pixels: u64) -> Result<Decoded, String> {
    let reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format().map_err(|e| e.to_string())?;
    let format = match reader.format() {
        Some(ImageFormat::Png) => "png",
        Some(ImageFormat::Jpeg) => "jpeg",
        Some(ImageFormat::Gif) => "gif",
        Some(ImageFormat::WebP) => "webp",
        _ => return Err("not a PNG, JPEG, GIF or WebP image".into()),
    };
    // The header alone first, so a decompression bomb is refused before any pixel is decoded.
    if let Some((w, h)) = header_size(bytes) {
        check_size(format, w, h, max_pixels)?;
    }
    let mut reader = reader;
    let mut limits = Limits::default();
    limits.max_alloc = Some(MAX_ALLOC);
    reader.limits(limits);
    let mut dec = reader.into_decoder().map_err(|e| format!("can't read the {format} image: {e}"))?;
    let (w, h) = dec.dimensions();
    check_size(format, w, h, max_pixels)?;
    let orientation = dec.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(dec).map_err(|e| format!("can't decode the {format} image: {e}"))?;
    img.apply_orientation(orientation);
    let rgba = img.into_rgba8();
    Ok(Decoded { format, width: rgba.width(), height: rgba.height(), rgba: rgba.into_raw() })
}

/// Width and height from the file header (PNG IHDR, GIF screen, WebP VP8/VP8L/VP8X, JPEG SOFn),
/// read without decoding. `None` if the header is not where it should be; the decoder then decides.
pub fn header_size(b: &[u8]) -> Option<(u32, u32)> {
    let be32 = |i: usize| Some(u32::from_be_bytes(b.get(i..i + 4)?.try_into().ok()?));
    let le16 = |i: usize| Some(u16::from_le_bytes(b.get(i..i + 2)?.try_into().ok()?) as u32);
    let le24 = |i: usize| Some(le16(i)? | (*b.get(i + 2)? as u32) << 16);
    if b.starts_with(b"\x89PNG\r\n\x1a\n") && b.get(12..16) == Some(b"IHDR") {
        return Some((be32(16)?, be32(20)?));
    }
    if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        return Some((le16(6)?, le16(8)?));
    }
    if b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP") {
        return match b.get(12..16)? {
            b"VP8X" => Some((le24(24)? + 1, le24(27)? + 1)),
            b"VP8L" => {
                let v = u32::from_le_bytes(b.get(21..25)?.try_into().ok()?);
                Some(((v & 0x3fff) + 1, ((v >> 14) & 0x3fff) + 1))
            }
            b"VP8 " => Some((le16(26)? & 0x3fff, le16(28)? & 0x3fff)),
            _ => None,
        };
    }
    if b.starts_with(&[0xff, 0xd8]) {
        let mut i = 2;
        while i + 4 <= b.len() {
            if b[i] != 0xff {
                return None;
            }
            let marker = b[i + 1];
            if marker == 0xff {
                i += 1;
                continue;
            }
            let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
            // SOF0..SOF15, except DHT (c4), JPG (c8) and DAC (cc).
            if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
                let h = u16::from_be_bytes([*b.get(i + 5)?, *b.get(i + 6)?]) as u32;
                let w = u16::from_be_bytes([*b.get(i + 7)?, *b.get(i + 8)?]) as u32;
                return Some((w, h));
            }
            if marker == 0xda || len < 2 {
                return None;
            }
            i += 2 + len;
        }
    }
    None
}

fn check_size(format: &str, w: u32, h: u32, max_pixels: u64) -> Result<(), String> {
    let pixels = w as u64 * h as u64;
    if w == 0 || h == 0 {
        return Err(format!("the {format} image is empty ({w}×{h})"));
    }
    if pixels > max_pixels {
        return Err(format!("the {format} image is {w}×{h} = {pixels} pixels, more than the {max_pixels} allowed (option max_pixels)"));
    }
    Ok(())
}

/// Height for `width`, keeping the aspect ratio (rounded half up, at least 1).
pub fn height_for(sw: u32, sh: u32, width: u32) -> u32 {
    let h = (2 * sh as u64 * width as u64 + sw as u64) / (2 * sw as u64);
    h.max(1) as u32
}

pub fn encode(rgba: &[u8], w: u32, h: u32, format: Format, quality: u8, background: [u8; 3]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let opaque = rgba.chunks_exact(4).all(|p| p[3] == 255);
    let rgb = |bg: [u8; 3]| -> Vec<u8> {
        rgba.chunks_exact(4)
            .flat_map(|p| {
                let a = p[3] as u32;
                let mix = |c: u8, b: u8| ((c as u32 * a + b as u32 * (255 - a) + 127) / 255) as u8;
                [mix(p[0], bg[0]), mix(p[1], bg[1]), mix(p[2], bg[2])]
            })
            .collect()
    };
    let res = match format {
        Format::Png => {
            let enc = PngEncoder::new_with_quality(&mut out, CompressionType::Default, FilterType::Adaptive);
            if opaque {
                enc.write_image(&rgb([0, 0, 0]), w, h, ExtendedColorType::Rgb8)
            } else {
                enc.write_image(rgba, w, h, ExtendedColorType::Rgba8)
            }
        }
        Format::Jpeg => JpegEncoder::new_with_quality(&mut out, quality).write_image(&rgb(background), w, h, ExtendedColorType::Rgb8),
        Format::Webp => {
            let enc = WebPEncoder::new_lossless(&mut out);
            if opaque {
                enc.write_image(&rgb([0, 0, 0]), w, h, ExtendedColorType::Rgb8)
            } else {
                enc.write_image(rgba, w, h, ExtendedColorType::Rgba8)
            }
        }
    };
    res.map_err(|e| format!("can't encode {}: {e}", format.name()))?;
    Ok(out)
}

struct Options {
    widths: Option<Vec<u32>>,
    formats: Option<Vec<Format>>,
    quality: u8,
    name: Option<String>,
    max_pixels: u64,
    background: [u8; 3],
}

fn options(req: &Json) -> Result<Options, String> {
    let empty = Json::Obj(Vec::new());
    let opts = req.get("options").unwrap_or(&empty);
    if let Json::Obj(fields) = opts {
        for (k, _) in fields {
            if !OPTIONS.contains(&k.as_str()) {
                return Err(format!("unknown option \"{k}\" (options: {})", OPTIONS.join(", ")));
            }
        }
    }
    let opt = |k: &str| opts.get(k).and_then(Json::as_str);
    let widths = match opt("widths") {
        None => None,
        Some(list) => {
            let mut ws = Vec::new();
            for part in list.split(',') {
                let p = part.trim();
                let w: u32 = p
                    .parse()
                    .ok()
                    .filter(|w| (1..=MAX_WIDTH).contains(w) && p.bytes().all(|b| b.is_ascii_digit()))
                    .ok_or_else(|| format!("option \"widths\" must be whole numbers from 1 to {MAX_WIDTH}, separated by commas; got \"{p}\""))?;
                ws.push(w);
            }
            ws.sort_unstable();
            ws.dedup();
            if ws.len() > MAX_WIDTHS {
                return Err(format!("option \"widths\" lists {} widths; at most {MAX_WIDTHS} are allowed", ws.len()));
            }
            Some(ws)
        }
    };
    let formats = match opt("format") {
        None => None,
        Some(list) => {
            let mut fs: Vec<Format> = Vec::new();
            for part in list.split(',') {
                let f = Format::parse(part.trim()).ok_or_else(|| format!("option \"format\" must be png, jpeg or webp (comma-separated); got \"{}\"", part.trim()))?;
                if !fs.contains(&f) {
                    fs.push(f);
                }
            }
            Some(fs)
        }
    };
    let quality = match opt("quality") {
        None => DEFAULT_QUALITY,
        Some(q) => q
            .parse::<u8>()
            .ok()
            .filter(|q| (1..=100).contains(q))
            .ok_or_else(|| format!("option \"quality\" must be a whole number from 1 to 100; got \"{q}\""))?,
    };
    let name = match opt("name") {
        None => None,
        Some(n) => {
            if n.is_empty() || n.starts_with('.') || !n.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')) {
                return Err(format!("option \"name\" may use only letters, digits, '-', '_' and '.' (not first); got \"{n}\""));
            }
            Some(n.to_string())
        }
    };
    let max_pixels = match opt("max_pixels") {
        None => MAX_PIXELS,
        Some(m) => m
            .parse::<u64>()
            .ok()
            .filter(|m| (1..=MAX_PIXELS).contains(m))
            .ok_or_else(|| format!("option \"max_pixels\" must be a whole number from 1 to {MAX_PIXELS}; got \"{m}\""))?,
    };
    let background = match opt("background") {
        None => [255, 255, 255],
        Some(c) => hex_color(c).ok_or_else(|| format!("option \"background\" must be #rgb or #rrggbb; got \"{c}\""))?,
    };
    Ok(Options { widths, formats, quality, name, max_pixels, background })
}

fn hex_color(c: &str) -> Option<[u8; 3]> {
    let h = c.strip_prefix('#')?;
    if !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let v = |s: &str| u8::from_str_radix(s, 16).ok();
    match h.len() {
        3 => {
            let d: Vec<u8> = h.chars().map(|c| v(&c.to_string()).unwrap() * 17).collect();
            Some([d[0], d[1], d[2]])
        }
        6 => Some([v(&h[0..2])?, v(&h[2..4])?, v(&h[4..6])?]),
        _ => None,
    }
}

/// The file name's last segment, and its image format if its extension names one.
fn dest_format(dest: &str) -> Option<Format> {
    let base = dest.rsplit('/').next().unwrap_or("");
    let ext = base.rsplit_once('.')?.1.to_ascii_lowercase();
    Format::parse(&ext)
}

fn stem(name: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or(name);
    let s = match base.rsplit_once('.') {
        Some((s, _)) if !s.is_empty() => s,
        _ => base,
    };
    let clean: String = s.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' }).collect();
    let clean = clean.trim_start_matches('.').to_string();
    if clean.is_empty() {
        "image".into()
    } else {
        clean
    }
}

pub fn transform(request: &[u8]) -> Result<Output, String> {
    let text = std::str::from_utf8(request).map_err(|_| "request is not UTF-8".to_string())?;
    let req = json::parse(text)?;
    let opts = options(&req)?;
    let input = req.get("input").ok_or("needs an image source: TRANSFORM ./photo.jpg WITH image TO …")?;
    let bytes = input.get("bytes").and_then(Json::as_str).ok_or("the request has no input bytes")?;
    let bytes = b64::decode(bytes).ok_or("the request's input bytes are not valid base64")?;
    let input_name = input.get("name").and_then(Json::as_str).unwrap_or("image");
    let dest = req.get("dest").and_then(Json::as_str).unwrap_or("/");

    let img = decode(&bytes, opts.max_pixels)?;
    let (sw, sh) = (img.width, img.height);
    // No upscaling: widths above the source's are dropped; if none is left, the source width is used.
    let mut widths: Vec<u32> = opts.widths.clone().unwrap_or_else(|| vec![sw]).into_iter().filter(|w| *w <= sw).collect();
    if widths.is_empty() {
        widths.push(sw);
    }

    if let Some(format) = dest_format(dest) {
        // One file: the destination's extension picks the format.
        if opts.formats.is_some() {
            return Err(format!("the destination '{dest}' is one file, so its extension picks the format; drop option \"format\" or write to a directory ('/img/')"));
        }
        if opts.widths.as_ref().is_some_and(|w| w.len() > 1) {
            return Err(format!("the destination '{dest}' is one file, so option \"widths\" takes one width; write to a directory ('/img/') for several"));
        }
        if opts.name.is_some() {
            return Err("option \"name\" names the files written to a directory destination; this destination is one file".into());
        }
        let w = widths[0];
        let h = height_for(sw, sh, w);
        let px = resize::resize(&img.rgba, sw, sh, w, h);
        return Ok(Output::Bytes(encode(&px, w, h, format, opts.quality, opts.background)?));
    }

    let formats = opts.formats.clone().unwrap_or_else(|| {
        vec![match img.format {
            "jpeg" => Format::Jpeg,
            "webp" => Format::Webp,
            _ => Format::Png,
        }]
    });
    let name = opts.name.clone().unwrap_or_else(|| stem(input_name));
    let base = dest.trim_end_matches('/');
    let mut files = Vec::new();
    // (format, width, height, path, size)
    let mut variants: Vec<(Format, u32, u32, String, usize)> = Vec::new();
    for &w in &widths {
        let h = height_for(sw, sh, w);
        let px = resize::resize(&img.rgba, sw, sh, w, h);
        for &f in &formats {
            let data = encode(&px, w, h, f, opts.quality, opts.background)?;
            let rel = format!("{name}-{w}.{}", f.ext());
            variants.push((f, w, h, format!("{base}/{rel}"), data.len()));
            files.push((rel, data));
        }
    }
    let meta = metadata(input_name, &img, &formats, &variants);
    files.push((format!("{name}.json"), meta.into_bytes()));
    Ok(Output::Files(files))
}

/// The metadata file (docs/PLUGINS.md, "The image plugin"); version 1.
fn metadata(input_name: &str, img: &Decoded, formats: &[Format], variants: &[(Format, u32, u32, String, usize)]) -> String {
    let srcset = |f: Format| -> String {
        variants.iter().filter(|v| v.0 == f).map(|v| format!("{} {}w", v.3, v.1)).collect::<Vec<_>>().join(", ")
    };
    let fallback = *formats.last().unwrap();
    let largest = variants.iter().filter(|v| v.0 == fallback).last().unwrap();
    let mut s = String::new();
    s.push_str("{\n  \"version\": 1,\n");
    s.push_str(&format!(
        "  \"source\": {{ \"name\": {}, \"format\": \"{}\", \"width\": {}, \"height\": {} }},\n",
        quote(input_name),
        img.format,
        img.width,
        img.height
    ));
    s.push_str(&format!("  \"width\": {},\n  \"height\": {},\n", largest.1, largest.2));
    s.push_str(&format!("  \"src\": {},\n", quote(&largest.3)));
    s.push_str(&format!("  \"type\": \"{}\",\n", fallback.mime()));
    s.push_str(&format!("  \"srcset\": {},\n", quote(&srcset(fallback))));
    s.push_str("  \"sources\": [\n");
    let sources: Vec<String> = formats.iter().map(|&f| format!("    {{ \"type\": \"{}\", \"srcset\": {} }}", f.mime(), quote(&srcset(f)))).collect();
    s.push_str(&sources.join(",\n"));
    s.push_str("\n  ],\n  \"variants\": [\n");
    let vs: Vec<String> = variants
        .iter()
        .map(|v| format!("    {{ \"path\": {}, \"type\": \"{}\", \"width\": {}, \"height\": {}, \"size\": {} }}", quote(&v.3), v.0.mime(), v.1, v.2, v.4))
        .collect();
    s.push_str(&vs.join(",\n"));
    s.push_str("\n  ]\n}\n");
    s
}

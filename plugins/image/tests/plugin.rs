//! Native tests of the plugin's request handling, decoders and limits, plus a
//! light mutation fuzz of every decoder. (The .wasm module runs the same code;
//! the Sitefile test suites check it under V8 and wasmtime.)

use sitefile_image::{b64, decode, encode, header_size, transform, Format, Output, MAX_PIXELS};
use std::path::Path;

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join(name)).unwrap()
}

fn request(name: &str, bytes: &[u8], dest: &str, options: &[(&str, &str)]) -> Vec<u8> {
    let opts: Vec<String> = options.iter().map(|(k, v)| format!("\"{k}\":\"{v}\"")).collect();
    format!(
        "{{\"apiVersion\":1,\"dest\":\"{dest}\",\"options\":{{{}}},\"input\":{{\"path\":\"./{name}\",\"name\":\"{name}\",\"bytes\":\"{}\"}}}}",
        opts.join(","),
        b64::encode(bytes)
    )
    .into_bytes()
}

fn files(name: &str, dest: &str, options: &[(&str, &str)]) -> Vec<(String, Vec<u8>)> {
    match transform(&request(name, &fixture(name), dest, options)) {
        Ok(Output::Files(f)) => f,
        Ok(Output::Bytes(_)) => panic!("expected files"),
        Err(e) => panic!("{e}"),
    }
}

fn error(name: &str, bytes: &[u8], dest: &str, options: &[(&str, &str)]) -> String {
    match transform(&request(name, bytes, dest, options)) {
        Err(e) => e,
        Ok(_) => panic!("expected an error"),
    }
}

#[test]
fn decodes_every_supported_format() {
    for (name, format, w, h) in [
        ("tiny.png", "png", 40, 30),
        ("tiny.jpg", "jpeg", 48, 32),
        ("tiny.gif", "gif", 30, 20),
        ("tiny-lossy.webp", "webp", 36, 24),
        // EXIF orientation 6 is applied: 48×32 stored, 32×48 shown.
        ("rotated.jpg", "jpeg", 32, 48),
    ] {
        let d = decode(&fixture(name), MAX_PIXELS).unwrap();
        assert_eq!((d.format, d.width, d.height), (format, w, h), "{name}");
        assert_eq!(d.rgba.len(), (w * h * 4) as usize);
    }
}

#[test]
fn header_sizes_match_the_decoders() {
    for (name, size) in [("tiny.png", (40, 30)), ("tiny.jpg", (48, 32)), ("tiny.gif", (30, 20)), ("tiny-lossy.webp", (36, 24)), ("rotated.jpg", (48, 32)), ("bomb.png", (60000, 60000))] {
        assert_eq!(header_size(&fixture(name)), Some(size), "{name}");
    }
    let px = decode(&fixture("tiny.png"), MAX_PIXELS).unwrap();
    let lossless = encode(&px.rgba, 40, 30, Format::Webp, 80, [255, 255, 255]).unwrap();
    assert_eq!(header_size(&lossless), Some((40, 30)));
    assert_eq!(header_size(b"\xff\xd8\xff"), None);
    assert_eq!(header_size(b""), None);
}

#[test]
fn a_decompression_bomb_is_refused_from_its_header() {
    let e = error("bomb.png", &fixture("bomb.png"), "/img/", &[]);
    assert!(e.contains("60000×60000 = 3600000000 pixels, more than the 40000000 allowed"), "{e}");
    let e = error("tiny.jpg", &fixture("tiny.jpg"), "/img/", &[("max_pixels", "1000")]);
    assert!(e.contains("48×32 = 1536 pixels, more than the 1000 allowed"), "{e}");
}

#[test]
fn a_directory_gets_one_file_per_width_and_format_plus_metadata() {
    let f = files("tiny.jpg", "/img", &[("widths", "24,16,100,16"), ("format", "webp,jpeg")]);
    let names: Vec<&str> = f.iter().map(|(n, _)| n.as_str()).collect();
    // 100 is wider than the source (48) and dropped; 16 is listed once.
    assert_eq!(names, ["tiny-16.webp", "tiny-16.jpg", "tiny-24.webp", "tiny-24.jpg", "tiny.json"]);
    for (n, b) in &f[..4] {
        let d = decode(b, MAX_PIXELS).unwrap();
        let w: u32 = n[5..7].parse().unwrap();
        assert_eq!((d.width, d.height), (w, if w == 16 { 11 } else { 16 }), "{n}");
    }
    let meta = String::from_utf8(f[4].1.clone()).unwrap();
    assert!(meta.starts_with("{\n  \"version\": 1,\n  \"source\": { \"name\": \"tiny.jpg\", \"format\": \"jpeg\", \"width\": 48, \"height\": 32 },\n  \"width\": 24,\n  \"height\": 16,\n  \"src\": \"/img/tiny-24.jpg\",\n  \"type\": \"image/jpeg\",\n  \"srcset\": \"/img/tiny-16.jpg 16w, /img/tiny-24.jpg 24w\",\n"), "{meta}");
    assert!(meta.contains("{ \"type\": \"image/webp\", \"srcset\": \"/img/tiny-16.webp 16w, /img/tiny-24.webp 24w\" }"), "{meta}");
    assert!(meta.contains(&format!("{{ \"path\": \"/img/tiny-24.jpg\", \"type\": \"image/jpeg\", \"width\": 24, \"height\": 16, \"size\": {} }}", f[3].1.len())), "{meta}");
}

#[test]
fn defaults_keep_the_size_and_the_format_family() {
    let names = |n: &str| files(n, "/", &[]).into_iter().map(|(n, _)| n).collect::<Vec<_>>();
    assert_eq!(names("tiny.jpg"), ["tiny-48.jpg", "tiny.json"]);
    assert_eq!(names("tiny.png"), ["tiny-40.png", "tiny.json"]);
    assert_eq!(names("tiny.gif"), ["tiny-30.png", "tiny.json"]);
    assert_eq!(names("tiny-lossy.webp"), ["tiny-lossy-36.webp", "tiny-lossy.json"]);
    let f = files("tiny.png", "/a/", &[("name", "logo")]);
    assert_eq!(f[0].0, "logo-40.png");
    // An unscaled PNG keeps its pixels exactly (and its alpha).
    let back = decode(&f[0].1, MAX_PIXELS).unwrap();
    assert_eq!(back.rgba, decode(&fixture("tiny.png"), MAX_PIXELS).unwrap().rgba);
}

#[test]
fn a_file_destination_picks_the_format_from_its_extension() {
    for (dest, magic) in [("/x.png", &b"\x89PNG"[..]), ("/x.jpg", b"\xff\xd8"), ("/x.JPEG", b"\xff\xd8"), ("/x.webp", b"RIFF")] {
        match transform(&request("tiny.png", &fixture("tiny.png"), dest, &[("widths", "20")])).unwrap() {
            Output::Bytes(b) => {
                assert!(b.starts_with(magic), "{dest}");
                let d = decode(&b, MAX_PIXELS).unwrap();
                assert_eq!((d.width, d.height), (20, 15));
            }
            Output::Files(_) => panic!("{dest}: expected one file"),
        }
    }
    assert!(error("tiny.png", &fixture("tiny.png"), "/x.png", &[("widths", "10,20")]).contains("takes one width"));
    assert!(error("tiny.png", &fixture("tiny.png"), "/x.png", &[("format", "webp")]).contains("extension picks the format"));
}

#[test]
fn options_are_checked() {
    let png = fixture("tiny.png");
    for (k, v, needle) in [
        ("widths", "0", "whole numbers from 1 to 16384"),
        ("widths", "20000", "whole numbers from 1 to 16384"),
        ("widths", "+5", "whole numbers"),
        ("widths", "1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17", "at most 16"),
        ("format", "avif", "must be png, jpeg or webp"),
        ("quality", "0", "from 1 to 100"),
        ("name", "../x", "only letters"),
        ("max_pixels", "50000000", "from 1 to 40000000"),
        ("background", "red", "#rgb or #rrggbb"),
        ("widht", "1", "unknown option \"widht\""),
    ] {
        let e = error("tiny.png", &png, "/img/", &[(k, v)]);
        assert!(e.contains(needle), "{k}={v}: {e}");
    }
    assert!(error("x.txt", b"hello", "/img/", &[]).contains("not a PNG, JPEG, GIF or WebP image"));
}

#[test]
fn the_same_request_gives_the_same_bytes() {
    let a = files("tiny.jpg", "/img/", &[("widths", "20,40"), ("format", "jpeg,png,webp"), ("quality", "70")]);
    let b = files("tiny.jpg", "/img/", &[("widths", "20,40"), ("format", "jpeg,png,webp"), ("quality", "70")]);
    assert_eq!(a, b);
}

/// Light fuzzing: mutated and truncated copies of every fixture must give
/// an error or an image, never a panic (which would trap the wasm module).
#[test]
fn mutated_images_never_panic() {
    let mut seed: u64 = 0x5eed_1e55;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut outcomes = (0, 0);
    for name in ["tiny.png", "tiny.jpg", "tiny.gif", "tiny-lossy.webp", "rotated.jpg", "bomb.png"] {
        let original = fixture(name);
        for round in 0..1500 {
            let mut b = original.clone();
            match round % 4 {
                0 => b.truncate((next() as usize) % b.len()),
                1 => {
                    for _ in 0..1 + next() % 4 {
                        let i = next() as usize % b.len();
                        b[i] ^= 1 << (next() % 8);
                    }
                }
                2 => {
                    let i = next() as usize % b.len();
                    b[i] = [0x00, 0xff, 0x7f, 0x80][next() as usize % 4];
                }
                _ => {
                    let i = next() as usize % b.len();
                    let extra: Vec<u8> = (0..next() % 16).map(|_| next() as u8).collect();
                    b.splice(i..i, extra);
                }
            }
            let req = request(name, &b, "/img/", &[("max_pixels", "200000"), ("widths", "8,16")]);
            match std::panic::catch_unwind(|| transform(&req).map(|_| ())) {
                Ok(Ok(())) => outcomes.0 += 1,
                Ok(Err(_)) => outcomes.1 += 1,
                Err(_) => panic!("{name}, round {round}: the plugin panicked on {:?}", b),
            }
        }
    }
    assert!(outcomes.0 > 0 && outcomes.1 > 0, "{outcomes:?}");
}

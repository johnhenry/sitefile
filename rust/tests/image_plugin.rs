//! The first-party image plugin (plugins/image, docs/PLUGINS.md "The image
//! plugin") under wasmtime, ported from test/image-plugin.test.ts. The output
//! digests are pinned in both suites, so V8 and wasmtime must agree byte for byte.

use sitefile::digest::sha256;
use sitefile::error::ErrorCode;
use sitefile::executor::{build, BuildOptions, LockfileSetting};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, Once};

/// Tree digest of examples/image (also pinned in test/image-plugin.test.ts).
const EXAMPLE_DIGEST: &str = "sha256:acd091f195830ad6a9059bd937dcfdfd7ed20ab76fb929e8b0538c7f029252f3";
/// tiny.jpg at widths 16,24 as jpeg, png and webp (directory mode).
const TINY_DIGEST: &str = "sha256:ab43d0d4f76f6f28cf1ce0876b71822985342702ae1568817401ca7239f8dfc0";

/// Tests run one at a time: the module is compiled once (debug Cranelift takes a
/// while for an 800 KB module) and then reused from the in-process cache. The
/// disk cache is switched off so nothing is written under the real
/// $SITEFILE_HOME; the variable is set before any test in this binary builds.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    static ONCE: Once = Once::new();
    let g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    ONCE.call_once(|| std::env::set_var("SITEFILE_NO_WASM_CACHE", "1"));
    g
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}
fn wasm() -> Vec<u8> {
    fs::read(repo_root().join("plugins/image/image.wasm")).unwrap()
}
fn testdata(name: &str) -> Vec<u8> {
    fs::read(repo_root().join("plugins/image/testdata").join(name)).unwrap()
}

struct Tmp(PathBuf);
impl Tmp {
    fn new() -> Tmp {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!("sitefile-image-rs-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        Tmp(fs::canonicalize(&p).unwrap())
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn site(line: &str, images: &[(&str, Vec<u8>)]) -> Tmp {
    let t = Tmp::new();
    fs::create_dir_all(t.0.join("plugins")).unwrap();
    fs::write(t.0.join("plugins/image.wasm"), wasm()).unwrap();
    for (name, bytes) in images {
        fs::write(t.0.join(name), bytes).unwrap();
    }
    fs::write(t.0.join("Sitefile"), format!("PLUGIN image FROM ./plugins/image.wasm\n{line}\nEXPORT /\n")).unwrap();
    t
}

fn options(dir: &Path) -> BuildOptions {
    BuildOptions { context_dir: dir.to_string_lossy().into_owned(), lockfile: LockfileSetting::Disabled, ..Default::default() }
}

fn paths(t: &sitefile::tree::Tree) -> Vec<String> {
    t.entries().map(|(p, _)| p.clone()).collect()
}

fn plugin_error(line: &str, images: &[(&str, Vec<u8>)]) -> String {
    let t = site(line, images);
    let e = build(&options(&t.0)).err().expect("expected an error");
    assert_eq!(e.code, ErrorCode::E_PLUGIN, "{}", e.message());
    e.message()
}

#[test]
fn the_example_builds_and_its_copy_of_the_plugin_is_the_pinned_one() {
    let _serial = serial();
    let bytes = wasm();
    let copy = fs::read(repo_root().join("examples/image/plugins/image.wasm")).unwrap();
    assert!(copy == bytes, "examples/image/plugins/image.wasm must equal plugins/image/image.wasm (run plugins/image/build.sh)");
    let sitefile = fs::read_to_string(repo_root().join("examples/image/Sitefile")).unwrap();
    assert!(sitefile.contains(&format!("INTEGRITY {}", sha256(&bytes))), "examples/image/Sitefile must pin the current image.wasm");

    let r = build(&options(&repo_root().join("examples/image"))).unwrap();
    assert_eq!(
        paths(&r.output),
        ["/gallery.html", "/img/badge-128.png", "/img/badge-128.webp", "/img/badge-64.png", "/img/badge-64.webp", "/img/badge.json", "/img/sky-1200.jpg", "/img/sky-400.jpg", "/img/sky-800.jpg", "/img/sky.json", "/img/thumbs/badge.png", "/img/thumbs/sky.png", "/index.html"]
    );
    let meta: serde_json::Value = serde_json::from_slice(&r.output.get("/img/sky.json").unwrap().bytes).unwrap();
    assert_eq!(meta["version"], 1);
    assert_eq!(meta["source"], serde_json::json!({ "name": "sky.jpg", "format": "jpeg", "width": 1600, "height": 1000 }));
    assert_eq!((meta["width"].as_u64(), meta["height"].as_u64()), (Some(1200), Some(750)));
    assert_eq!(meta["srcset"], "/img/sky-400.jpg 400w, /img/sky-800.jpg 800w, /img/sky-1200.jpg 1200w");
    for v in meta["variants"].as_array().unwrap() {
        let p = v["path"].as_str().unwrap();
        assert_eq!(r.output.get(p).unwrap().bytes.len() as u64, v["size"].as_u64().unwrap(), "{p}");
    }
    assert_eq!(r.output.digest(), EXAMPLE_DIGEST);
}

#[test]
fn every_supported_input_format_several_widths_and_output_formats() {
    let _serial = serial();
    let t = site("TRANSFORM ./tiny.jpg WITH image TO /out/ OPTIONS widths=16,24,100 format=jpeg,png,webp quality=70", &[("tiny.jpg", testdata("tiny.jpg"))]);
    let r = build(&options(&t.0)).unwrap();
    assert_eq!(paths(&r.output), ["/out/tiny-16.jpg", "/out/tiny-16.png", "/out/tiny-16.webp", "/out/tiny-24.jpg", "/out/tiny-24.png", "/out/tiny-24.webp", "/out/tiny.json"]);
    assert_eq!(r.output.digest(), TINY_DIGEST);

    for (name, w, h) in [("tiny.png", 40, 30), ("tiny.gif", 30, 20), ("tiny-lossy.webp", 36, 24), ("rotated.jpg", 32, 48)] {
        let t = site(&format!("TRANSFORM ./{name} WITH image TO /out/ OPTIONS name=x"), &[(name, testdata(name))]);
        let r = build(&options(&t.0)).unwrap();
        let meta: serde_json::Value = serde_json::from_slice(&r.output.get("/out/x.json").unwrap().bytes).unwrap();
        assert_eq!((meta["width"].as_u64(), meta["height"].as_u64()), (Some(w), Some(h)), "{name}");
    }
}

#[test]
fn a_bomb_an_unknown_option_and_a_non_image_fail_with_e_plugin() {
    let _serial = serial();
    let bomb = plugin_error("TRANSFORM ./bomb.png WITH image TO /out/", &[("bomb.png", testdata("bomb.png"))]);
    assert!(bomb.contains("PLUGIN image: the png image is 60000×60000 = 3600000000 pixels, more than the 40000000 allowed"), "{bomb}");
    let e = plugin_error("TRANSFORM ./tiny.png WITH image TO /out/ OPTIONS widht=10", &[("tiny.png", testdata("tiny.png"))]);
    assert!(e.contains("unknown option \"widht\""), "{e}");
    let e = plugin_error("TRANSFORM ./x.png WITH image TO /out/", &[("x.png", b"not an image".to_vec())]);
    assert!(e.contains("not a PNG, JPEG, GIF or WebP image"), "{e}");
}

#[test]
fn results_are_cached_by_content_digest() {
    let _serial = serial();
    let t = site("TRANSFORM ./tiny.png WITH image TO /out/ OPTIONS widths=10,20", &[("tiny.png", testdata("tiny.png"))]);
    let cache = Tmp::new();
    let opts = BuildOptions { cache_dir: Some(cache.0.clone()), ..options(&t.0) };
    let first = build(&opts).unwrap();
    assert_eq!(first.transform_stats.cache.misses, 1);
    let second = build(&opts).unwrap();
    assert_eq!(second.transform_stats.cache.hits, 1);
    assert_eq!(second.transform_stats.plugins_spawned, 0);
    assert_eq!(second.output.digest(), first.output.digest());
}

#[test]
fn light_fuzzing_under_wasmtime() {
    let _serial = serial();
    let mut seed: u32 = 0x1234567;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    for name in ["tiny.png", "tiny.jpg", "tiny.gif", "tiny-lossy.webp"] {
        let original = testdata(name);
        for round in 0..6 {
            let mut b = original.clone();
            if round % 2 == 0 {
                b.truncate(next() as usize % b.len());
            } else {
                let v = next() as u8;
                let i = next() as usize % b.len();
                b[i] = v;
            }
            let t = site("TRANSFORM ./in WITH image TO /out/ OPTIONS widths=8 max_pixels=100000", &[("in", b)]);
            match build(&options(&t.0)) {
                Ok(r) => assert!(r.output.get("/out/in.json").is_some(), "{name} round {round}"),
                Err(e) => assert_eq!(e.code, ErrorCode::E_PLUGIN, "{name} round {round}: {}", e.message()),
            }
        }
    }
}

//! Compiles the vendored Google Brotli encoder (vendor/brotli, v1.2.0, the version
//! Node 26 bundles) for `PRECOMPRESS br` (src/brotli.rs, johnhenry/sitefile#203).
//! Needs a C compiler (cc, clang or MSVC), as `ring` already does.

fn main() {
    let root = std::path::Path::new("vendor/brotli/c");
    let mut build = cc::Build::new();
    build.include(root.join("include")).warnings(false);
    for dir in ["common", "enc"] {
        let mut files: Vec<_> = std::fs::read_dir(root.join(dir))
            .expect("vendor/brotli is missing")
            .map(|e| e.expect("vendor/brotli").path())
            .filter(|p| p.extension().is_some_and(|x| x == "c"))
            .collect();
        files.sort();
        build.files(files);
    }
    build.compile("sitefile_brotlienc");
    // Windows gives the main thread a 1 MiB stack (Linux and macOS: 8 MiB), which
    // deep plugin/render paths overflow; ask for the same 8 MiB (#318).
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/STACK:8388608");
    }
    println!("cargo:rerun-if-changed=vendor/brotli");
    println!("cargo:rerun-if-changed=build.rs");
}

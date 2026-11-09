# Vendored Google Brotli (encoder) 1.2.0

The C encoder of [google/brotli](https://github.com/google/brotli), tag `v1.2.0`
(commit `028fb5a23661f123017c060daa546b55cf4bde29`), the version Node 26 bundles
(`process.versions.brotli`). `PRECOMPRESS br` (johnhenry/sitefile#203) compiles it with
`rust/build.rs` so the Rust port writes the same `.br` bytes as node:zlib's
`brotliCompressSync` in the TypeScript implementation.

Copied unchanged: `c/common/*.{c,h}`, `c/enc/*.{c,h}`, `c/include/brotli/*.h` and
`LICENSE` (MIT). Not copied: the decoder (`c/dec`), `c/common/dictionary.bin*` (the
dictionary is compiled in from `dictionary.c`) and `c/enc/static_init_lazy.cc` (C++,
only for `BROTLI_STATIC_INIT_LAZY`; the default is none).

To update: replace these files from the tag Node bundles (and `VERSION` in
`rust/src/brotli.rs`, `BROTLI_VERSION` in `src/precompress.ts`), then run
`node rust/tests/fixtures/brotli/corpus.mjs`, `cargo test brotli` and `npm test`.

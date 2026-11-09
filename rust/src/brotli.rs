//! Brotli compression for `PRECOMPRESS br` (johnhenry/sitefile#203): Google's C encoder,
//! vendored at the version Node 26 bundles (`vendor/brotli`, 1.2.0, compiled by `build.rs`),
//! driven exactly as node:zlib's `brotliCompressSync` drives it, so `src/precompress.ts`
//! and this module write the same bytes.

use std::os::raw::{c_int, c_void};

/// The settings both implementations use (`BROTLI_SETTINGS` in src/precompress.ts).
pub const QUALITY: u32 = 11;
pub const LGWIN: u32 = 22;
/// `BROTLI_MODE_GENERIC`.
pub const MODE: u32 = 0;

/// The vendored encoder's version, as `process.versions.brotli` prints it.
pub const VERSION: &str = "1.2.0";

#[repr(C)]
struct EncoderState {
    _private: [u8; 0],
}

const PARAM_MODE: c_int = 0;
const PARAM_QUALITY: c_int = 1;
const PARAM_LGWIN: c_int = 2;
const OPERATION_FINISH: c_int = 2;

extern "C" {
    fn BrotliEncoderCreateInstance(
        alloc: Option<unsafe extern "C" fn(*mut c_void, usize) -> *mut c_void>,
        free: Option<unsafe extern "C" fn(*mut c_void, *mut c_void)>,
        opaque: *mut c_void,
    ) -> *mut EncoderState;
    fn BrotliEncoderSetParameter(state: *mut EncoderState, param: c_int, value: u32) -> c_int;
    fn BrotliEncoderCompressStream(
        state: *mut EncoderState,
        op: c_int,
        available_in: *mut usize,
        next_in: *mut *const u8,
        available_out: *mut usize,
        next_out: *mut *mut u8,
        total_out: *mut usize,
    ) -> c_int;
    fn BrotliEncoderIsFinished(state: *mut EncoderState) -> c_int;
    fn BrotliEncoderHasMoreOutput(state: *mut EncoderState) -> c_int;
    fn BrotliEncoderDestroyInstance(state: *mut EncoderState);
    fn BrotliEncoderVersion() -> u32;
}

/// Output buffer size of a Node zlib stream (`zlib.constants.Z_DEFAULT_CHUNK`).
const CHUNK: usize = 16 * 1024;

/// `data` compressed with Brotli at [`QUALITY`], [`LGWIN`] and [`MODE`]: one
/// `BROTLI_OPERATION_FINISH` over the whole input, as `brotliCompressSync` does.
pub fn brotli_compress(data: &[u8]) -> Vec<u8> {
    // SAFETY: the state is created, used and destroyed here only; the input and
    // output pointers and their lengths come from live slices, and the encoder
    // never reads or writes past the lengths it is given.
    unsafe {
        let s = BrotliEncoderCreateInstance(None, None, std::ptr::null_mut());
        assert!(!s.is_null(), "brotli: out of memory");
        for (p, v) in [
            (PARAM_QUALITY, QUALITY),
            (PARAM_LGWIN, LGWIN),
            (PARAM_MODE, MODE),
        ] {
            assert!(BrotliEncoderSetParameter(s, p, v) != 0);
        }
        let mut out = Vec::new();
        let mut buf = vec![0u8; CHUNK];
        let mut available_in = data.len();
        let mut next_in = data.as_ptr();
        loop {
            let mut available_out = CHUNK;
            let mut next_out = buf.as_mut_ptr();
            let ok = BrotliEncoderCompressStream(
                s,
                OPERATION_FINISH,
                &mut available_in,
                &mut next_in,
                &mut available_out,
                &mut next_out,
                std::ptr::null_mut(),
            );
            if ok == 0 {
                BrotliEncoderDestroyInstance(s);
                panic!("brotli: encoder error");
            }
            out.extend_from_slice(&buf[..CHUNK - available_out]);
            if BrotliEncoderIsFinished(s) != 0 && BrotliEncoderHasMoreOutput(s) == 0 {
                break;
            }
        }
        BrotliEncoderDestroyInstance(s);
        out
    }
}

/// The vendored encoder's version (`BrotliEncoderVersion`), e.g. "1.2.0".
pub fn encoder_version() -> String {
    // SAFETY: a pure function of the compiled library.
    let v = unsafe { BrotliEncoderVersion() };
    format!("{}.{}.{}", v >> 24, (v >> 12) & 0xfff, v & 0xfff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vendored_encoder_is_the_version_node_bundles() {
        assert_eq!(encoder_version(), VERSION);
    }

    /// The same digests as test/precompress.test.ts pins for node:zlib.
    #[test]
    fn bytes_match_node_zlib() {
        use sha2::{Digest, Sha256};
        let hex = |b: &[u8]| crate::sha1::hex(&Sha256::digest(b));
        let a = "body { color: red; }\n".repeat(100);
        let b: Vec<String> = (0u64..20000).map(|i| to_base36(i * i)).collect();
        assert_eq!(
            hex(&brotli_compress(a.as_bytes())),
            "3645b0cee7d067c4e17a71f145f89ebe78d5b585b19cb43c8ea413cde27fb6b3"
        );
        assert_eq!(
            hex(&brotli_compress(b.join("\n").as_bytes())),
            "b56d7c74f4c412d2821feea91972faf1e823c80b45c61291dc3e4cbaea15db3a"
        );
        // The empty input is the 1-byte empty stream, as node:zlib writes it.
        assert_eq!(brotli_compress(b""), [0x3b]);
    }

    fn to_base36(mut n: u64) -> String {
        let mut d = Vec::new();
        loop {
            d.push(b"0123456789abcdefghijklmnopqrstuvwxyz"[(n % 36) as usize]);
            n /= 36;
            if n == 0 {
                break;
            }
        }
        d.reverse();
        String::from_utf8(d).unwrap()
    }

    /// Writes `<out>/<n>.br` for every path listed in `$SITEFILE_BROTLI_LIST`, numbered from 0:
    /// the corpus check against node:zlib (rust/tests/fixtures/brotli/corpus.mjs).
    #[test]
    #[ignore]
    fn brotli_corpus_external() {
        let list = std::env::var("SITEFILE_BROTLI_LIST").expect("SITEFILE_BROTLI_LIST");
        let out = std::env::var("SITEFILE_BROTLI_OUT").expect("SITEFILE_BROTLI_OUT");
        for (i, p) in std::fs::read_to_string(list).unwrap().lines().enumerate() {
            let data = std::fs::read(p).unwrap();
            std::fs::write(format!("{out}/{i}.br"), brotli_compress(&data)).unwrap();
        }
    }
}

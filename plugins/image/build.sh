#!/bin/sh
# Rebuild image.wasm from source and say whether it matches the committed one.
#
# Needs Rust with the wasm32-unknown-unknown target (rustup target add
# wasm32-unknown-unknown). The committed image.wasm was built with the
# toolchain below; another rustc version gives a working module with
# different bytes (and so a different INTEGRITY digest and cache key).
#
#   ./build.sh            build and copy to image.wasm and examples/image/plugins/
#   ./build.sh --check    build and only compare with the committed image.wasm
set -eu
cd "$(dirname "$0")"
TOOLCHAIN="rustc 1.93.1 (01f6ddf75 2026-02-11)"

have=$(rustc --version)
if [ "$have" != "$TOOLCHAIN" ]; then
  echo "note: building with '$have'; image.wasm was built with '$TOOLCHAIN'" >&2
fi

# Paths in panic messages would otherwise embed this machine's home directory.
sysroot=$(rustc --print sysroot)
cargo_home=${CARGO_HOME:-$HOME/.cargo}
RUSTFLAGS="--remap-path-prefix=$sysroot=/rustc --remap-path-prefix=$cargo_home=/cargo --remap-path-prefix=$(pwd)=/sitefile-image" \
  cargo build --release --target wasm32-unknown-unknown --locked --offline
built=target/wasm32-unknown-unknown/release/sitefile_image.wasm

sha() {
  if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}
new=$(sha "$built")
old=none
[ -f image.wasm ] && old=$(sha image.wasm)
echo "built   sha256:$new"
echo "in repo sha256:$old"
if [ "${1:-}" = "--check" ]; then
  [ "$new" = "$old" ] && echo "reproduced: identical to the committed image.wasm" && exit 0
  echo "differs from the committed image.wasm" >&2
  exit 1
fi
cp "$built" image.wasm
cp "$built" ../../examples/image/plugins/image.wasm
sed "s/INTEGRITY sha256:[0-9a-f]*/INTEGRITY sha256:$new/" ../../examples/image/Sitefile > ../../examples/image/Sitefile.tmp
mv ../../examples/image/Sitefile.tmp ../../examples/image/Sitefile
echo "wrote image.wasm, examples/image/plugins/image.wasm and the INTEGRITY pin in examples/image/Sitefile"

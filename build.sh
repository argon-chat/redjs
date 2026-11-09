#!/usr/bin/env bash
# Native Unix build and the same runtimes/{rid}/native + lib/{rid} layout as
# build.ps1. Optional Cargo features are passed through explicitly.
set -euo pipefail
root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
case "$(uname -s)/$(uname -m)" in
  Linux/x86_64) target=x86_64-unknown-linux-gnu; rid=linux-x64; platform=linux-amd64; extension=so ;;
  Darwin/x86_64) target=x86_64-apple-darwin; rid=osx-x64; platform=macos-amd64; extension=dylib ;;
  Darwin/arm64) target=aarch64-apple-darwin; rid=osx-arm64; platform=macos-arm64; extension=dylib ;;
  *) echo 'No pinned redjs prebuilt for this platform' >&2; exit 1 ;;
esac
export BUN_WEBKIT_DIR="${BUN_WEBKIT_DIR:-$root/prebuilt/bun-webkit-$platform}"
if [[ "$extension" == dylib ]]; then
  export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-13.0}"
fi
run_tests=false
shared=false
if [[ "$extension" == dylib ]]; then shared=true; fi
args=()
for argument in "$@"; do
  case "$argument" in
    --test) run_tests=true ;;
    --shared) shared=true ;;
    *) args+=("$argument") ;;
  esac
done
cd "$root/redjs"
crate_types=rlib,staticlib
if "$shared"; then crate_types+=,cdylib; fi
build_log="$(mktemp)"
trap 'rm -f -- "$build_log"' EXIT
cargo rustc --release --locked --target "$target" --crate-type "$crate_types" --no-default-features "${args[@]}" -- --print native-static-libs 2>&1 | tee "$build_log"
if "$run_tests"; then
  export RUST_TEST_THREADS="${RUST_TEST_THREADS:-1}"
  cargo test --release --locked --target "$target" --no-default-features "${args[@]}"
fi
target_dir="$(cargo metadata --locked --no-deps --format-version 1 | bun "$root/scripts/native-link-args.ts" --target-directory)"
mkdir -p "$root/runtimes/$rid/native" "$root/lib/$rid"
if "$shared"; then
  cp "$target_dir/$target/release/libredjs.$extension" "$root/runtimes/$rid/native/"
fi
cp "$target_dir/$target/release/libredjs.a" "$root/lib/$rid/"
bun "$root/scripts/native-link-args.ts" "$build_log" "$root/lib/$rid/native-static-libs.txt"
echo "[redjs] staged $rid libraries (shared=$shared)"

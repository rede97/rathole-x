#!/bin/bash
# rathole-x local distribution build. Builds the musl release binaries with
# the zig-cc toolchain (see docs/build-guide.md, "Cross-compiling and Testing
# for ARM") and packages them like the CI release artifacts:
#
#   dist/rathole-x-<target>.tar.gz
#   dist/sha256sums.txt
#
# Usage (from the repo root):
#   scripts/dist.sh                      # default targets below
#   TARGETS="aarch64-unknown-linux-musl" scripts/dist.sh
#
# i686/aarch64 need their zig-cc shims (one `ln` each, see build-guide.md);
# Windows artifacts are CI-only (release.yml), never built here.
set -euo pipefail

TARGETS=${TARGETS:-"x86_64-unknown-linux-musl armv7-unknown-linux-musleabihf"}
FEATURES="server,client,rustls,noise,websocket-rustls,hot-reload"
DIST=dist

fail() { echo "FAIL: $*" >&2; exit 1; }

mkdir -p "$DIST"
rm -f "$DIST/sha256sums.txt"
for target in $TARGETS; do
    echo "=== $target ==="
    cargo build --locked --release --target "$target" \
        --no-default-features --features "$FEATURES"
    bin="target/$target/release/rathole-x"
    [ -x "$bin" ] || fail "$bin missing after build"
    file "$bin" | grep -q 'statically linked' \
        || fail "$bin is not a static binary: $(file "$bin")"
    tar -czf "$DIST/rathole-x-$target.tar.gz" -C "target/$target/release" rathole-x
    (cd "$DIST" && sha256sum "rathole-x-$target.tar.gz" >> sha256sums.txt)
done

echo "=== artifacts ==="
ls -l "$DIST"

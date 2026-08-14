#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 1 ]; then
    echo "Usage: ./scripts/release.sh <TAG>" >&2
    exit 1
fi

TAG="$1"
TARGET="x86_64-pc-windows-msvc"
ZIP="rathole-x-${TARGET}.zip"
EXE="target/${TARGET}/release/rathole-x.exe"

echo "Building rathole-x for ${TARGET}..."
cargo build --release --target "${TARGET}"

echo "Creating ${ZIP}..."
powershell.exe -NoProfile -Command "Compress-Archive -Force -Path '${EXE}' -DestinationPath '${ZIP}'"

echo "Artifact: ${ZIP}"

if command -v gh >/dev/null 2>&1; then
    echo "Creating draft release ${TAG}..."
    gh release create "${TAG}" --draft --generate-notes --repo rede97/rathole-x "${ZIP}"
else
    echo "gh not found; skipping release creation."
    echo "To publish manually: gh release create \"${TAG}\" --draft --generate-notes --repo rede97/rathole-x \"${ZIP}\""
fi

#!/usr/bin/env bash
# Build aetherium for Linux (X11) and package a tarball in dist/.
# System deps (Debian/Ubuntu): sudo apt install libxkbcommon-dev libxkbcommon-x11-dev libfontconfig-dev
# Usage: ./scripts/build-linux.sh [--debug]   (default: --release)
set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE=release
[[ "${1:-}" == "--debug" ]] && PROFILE=debug

ARGS=(--locked)
[[ "$PROFILE" == release ]] && ARGS+=(--release)
cargo build "${ARGS[@]}"

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
ARCH=$(uname -m)
STAGE="dist/aetherium-linux-${ARCH}"
rm -rf "$STAGE"
mkdir -p "$STAGE"
cp "target/$PROFILE/aetherium" "$STAGE/"
cp README.md "$STAGE/"
TARBALL="dist/aetherium-${VERSION}-linux-${ARCH}.tar.gz"
tar -C dist -czf "$TARBALL" "$(basename "$STAGE")"
echo "built $TARBALL ($PROFILE)"

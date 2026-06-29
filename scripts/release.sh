#!/usr/bin/env bash
# Build release binaries, generate checksums, and package a Linux tarball.
#
# Usage: scripts/release.sh [version]
#
# Outputs to dist/<version>/:
#   fs2-cli, fs2-backend, fs2-daemon  — release binaries
#   SHA256SUMS                        — checksums for all artifacts
#   fs2-<version>-linux-x86_64.tar.gz — tarball with binaries + docs
#
# Requires: cargo, sha256sum, tar.
set -euo pipefail

VERSION="${1:-$(cargo metadata --no-deps --format-version 1 2>/dev/null | grep -o '"version":"[^"]*"' | head -1 | cut -d'"' -f4 || echo '0.1.0')}"
DIST="dist/${VERSION}"

echo "Building release binaries (version ${VERSION})..."
cargo build --release

mkdir -p "${DIST}"

# Copy binaries.
for bin in fs2-cli fs2-backend fs2-daemon; do
  cp "target/release/${bin}" "${DIST}/"
done

# Generate checksums.
echo "Generating checksums..."
(cd "${DIST}" && sha256sum fs2-cli fs2-backend fs2-daemon > SHA256SUMS)

# Build a Linux tarball with binaries + selected docs.
echo "Packaging Linux tarball..."
TARBALL="fs2-${VERSION}-linux-x86_64.tar.gz"
STAGING="$(mktemp -d)"
mkdir -p "${STAGING}/docs"
cp "${DIST}/fs2-cli" "${DIST}/fs2-backend" "${DIST}/fs2-daemon" "${DIST}/SHA256SUMS" "${STAGING}/"
cp README.md SECURITY.md "${STAGING}/"
cp docs/install-guide.md docs/safety-guide.md docs/workflow-guide.md "${STAGING}/docs/"
tar -czf "${DIST}/${TARBALL}" -C "${STAGING}" .
rm -rf "${STAGING}"

echo "Release artifacts in ${DIST}/:"
ls -la "${DIST}/"
echo "Done."

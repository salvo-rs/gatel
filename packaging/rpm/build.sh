#!/usr/bin/env bash
set -euo pipefail

# Build an RPM package for gatel.
#
# Usage: ./build.sh <path-to-gatel-binary> <version-tag>
#   e.g.: ./build.sh ./target/release/gatel v0.1.0

BINARY="${1:?Usage: build.sh <binary-path> <version>}"
VERSION="${2#v}"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BUILD_DIR="$(mktemp -d)"
trap 'rm -rf "$BUILD_DIR"' EXIT

echo "==> Building RPM: gatel ${VERSION}"

# Set up rpmbuild tree
for d in BUILD RPMS SOURCES SPECS SRPMS; do
    mkdir -p "${BUILD_DIR}/${d}"
done

# Create source tarball
SRC_DIR="${BUILD_DIR}/SOURCES/gatel-${VERSION}"
mkdir -p "${SRC_DIR}"
install -m 755 "$BINARY" "${SRC_DIR}/gatel"
cp "${SCRIPT_DIR}/gatel.service" "${SRC_DIR}/"

cd "${BUILD_DIR}/SOURCES"
tar czf "gatel-${VERSION}.tar.gz" "gatel-${VERSION}"

# Generate spec file
sed "s/{{VERSION}}/${VERSION}/g" \
    "${SCRIPT_DIR}/gatel.spec" > "${BUILD_DIR}/SPECS/gatel.spec"

rpmbuild --define "_topdir ${BUILD_DIR}" -bb "${BUILD_DIR}/SPECS/gatel.spec"

# Copy result
find "${BUILD_DIR}/RPMS" -name "*.rpm" -exec cp {} . \;
echo "==> RPM built successfully"

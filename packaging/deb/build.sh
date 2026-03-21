#!/usr/bin/env bash
set -euo pipefail

# Build a .deb package for gatel.
#
# Usage: ./build.sh <path-to-gatel-binary> <version-tag>
#   e.g.: ./build.sh ./target/release/gatel v0.1.0

BINARY="${1:?Usage: build.sh <binary-path> <version>}"
VERSION="${2#v}"  # strip leading 'v'

ARCH="$(dpkg --print-architecture 2>/dev/null || echo amd64)"
PKG_NAME="gatel"
PKG_DIR="${PKG_NAME}_${VERSION}_${ARCH}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

echo "==> Building DEB: ${PKG_NAME} ${VERSION} (${ARCH})"

rm -rf "$PKG_DIR"
mkdir -p "${PKG_DIR}/DEBIAN"
mkdir -p "${PKG_DIR}/usr/local/bin"
mkdir -p "${PKG_DIR}/etc/gatel"
mkdir -p "${PKG_DIR}/lib/systemd/system"
mkdir -p "${PKG_DIR}/var/log/gatel"
mkdir -p "${PKG_DIR}/var/lib/gatel"

# Binary
install -m 755 "$BINARY" "${PKG_DIR}/usr/local/bin/gatel"

# Default config
cat > "${PKG_DIR}/etc/gatel/gatel.kdl" <<'EOF'
global {
    log level="info"
    http ":80"
}

site "*" {
    route "/*" {
        respond "Hello from gatel!" status=200
    }
}
EOF

# Systemd unit
cp "${SCRIPT_DIR}/gatel.service" "${PKG_DIR}/lib/systemd/system/"

# Control file
sed "s/{{VERSION}}/${VERSION}/g; s/{{ARCH}}/${ARCH}/g" \
    "${SCRIPT_DIR}/control" > "${PKG_DIR}/DEBIAN/control"

# Maintainer scripts
for script in postinst prerm postrm; do
    if [[ -f "${SCRIPT_DIR}/${script}" ]]; then
        install -m 755 "${SCRIPT_DIR}/${script}" "${PKG_DIR}/DEBIAN/${script}"
    fi
done

# Mark config file as conffile (preserve on upgrade)
echo "/etc/gatel/gatel.kdl" > "${PKG_DIR}/DEBIAN/conffiles"

dpkg-deb --build "$PKG_DIR"
echo "==> Built: ${PKG_DIR}.deb"

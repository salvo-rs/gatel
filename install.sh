#!/usr/bin/env bash
set -euo pipefail

# Gatel installer for Linux and macOS
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/salvo-rs/gatel/main/install.sh | bash
#   or: ./install.sh [--prefix /usr/local] [--from-source]

REPO="salvo-rs/gatel"
PREFIX="${PREFIX:-/usr/local}"
FROM_SOURCE=false
VERSION="${VERSION:-latest}"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

info()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn()  { printf '\033[1;33mWARN:\033[0m %s\n' "$*" >&2; }
error() { printf '\033[1;31mERROR:\033[0m %s\n' "$*" >&2; exit 1; }

need_cmd() {
    if ! command -v "$1" &>/dev/null; then
        error "required command '$1' not found"
    fi
}

detect_os() {
    local os
    os="$(uname -s)"
    case "$os" in
        Linux*)  echo "linux" ;;
        Darwin*) echo "macos" ;;
        *)       error "unsupported operating system: $os" ;;
    esac
}

detect_arch() {
    local arch
    arch="$(uname -m)"
    case "$arch" in
        x86_64|amd64)   echo "x86_64" ;;
        aarch64|arm64)  echo "aarch64" ;;
        *)              error "unsupported architecture: $arch" ;;
    esac
}

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------

while [[ $# -gt 0 ]]; do
    case "$1" in
        --prefix)       PREFIX="$2"; shift 2 ;;
        --from-source)  FROM_SOURCE=true; shift ;;
        --version)      VERSION="$2"; shift 2 ;;
        -h|--help)
            cat <<EOF
Gatel installer

Usage: install.sh [OPTIONS]

Options:
  --prefix <DIR>    Installation prefix (default: /usr/local)
  --from-source     Build from source instead of downloading a binary
  --version <VER>   Install a specific version (default: latest)
  -h, --help        Show this help
EOF
            exit 0
            ;;
        *) error "unknown option: $1" ;;
    esac
done

# ---------------------------------------------------------------------------
# Install from source
# ---------------------------------------------------------------------------

install_from_source() {
    info "Installing gatel from source"

    need_cmd cargo
    need_cmd git

    local tmpdir
    tmpdir="$(mktemp -d)"
    trap 'rm -rf "$tmpdir"' EXIT

    info "Cloning repository..."
    if [[ "$VERSION" == "latest" ]]; then
        git clone --depth 1 "https://github.com/${REPO}.git" "$tmpdir/gatel"
    else
        git clone --depth 1 --branch "$VERSION" "https://github.com/${REPO}.git" "$tmpdir/gatel"
    fi

    cd "$tmpdir/gatel"

    info "Building release binaries..."
    cargo build --release

    install_binaries "target/release"
    install_extras
    info "Done! Run 'gatel --help' to get started."
}

# ---------------------------------------------------------------------------
# Install from prebuilt binary
# ---------------------------------------------------------------------------

install_from_binary() {
    local os arch
    os="$(detect_os)"
    arch="$(detect_arch)"

    need_cmd curl
    need_cmd tar

    info "Detected: ${os}/${arch}"

    local download_url tag asset_name
    if [[ "$VERSION" == "latest" ]]; then
        tag="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
            | grep '"tag_name"' | head -1 | cut -d'"' -f4)"
        if [[ -z "$tag" ]]; then
            warn "No prebuilt release found. Falling back to source build."
            install_from_source
            return
        fi
    else
        tag="$VERSION"
    fi

    asset_name="gatel-${tag}-${arch}-${os}.tar.gz"
    download_url="https://github.com/${REPO}/releases/download/${tag}/${asset_name}"

    info "Downloading gatel ${tag} for ${os}/${arch}..."

    local tmpdir
    tmpdir="$(mktemp -d)"
    trap 'rm -rf "$tmpdir"' EXIT

    if ! curl -fsSL -o "$tmpdir/gatel.tar.gz" "$download_url"; then
        warn "Binary download failed. Falling back to source build."
        install_from_source
        return
    fi

    info "Extracting..."
    tar -xzf "$tmpdir/gatel.tar.gz" -C "$tmpdir"

    install_binaries "$tmpdir"
    install_extras
    info "Installed gatel ${tag}"
    info "Run 'gatel --help' to get started."
}

# ---------------------------------------------------------------------------
# Common installation steps
# ---------------------------------------------------------------------------

install_binaries() {
    local src_dir="$1"
    local bin_dir="${PREFIX}/bin"

    info "Installing binaries to ${bin_dir}"

    if [[ -w "$bin_dir" ]] || mkdir -p "$bin_dir" 2>/dev/null; then
        for bin in gatel gatel-passwd gatel-precompress; do
            if [[ -f "${src_dir}/${bin}" ]]; then
                install -m 755 "${src_dir}/${bin}" "${bin_dir}/${bin}"
            fi
        done
    else
        info "Elevated permissions required to install to ${bin_dir}"
        sudo mkdir -p "$bin_dir"
        for bin in gatel gatel-passwd gatel-precompress; do
            if [[ -f "${src_dir}/${bin}" ]]; then
                sudo install -m 755 "${src_dir}/${bin}" "${bin_dir}/${bin}"
            fi
        done
    fi
}

install_extras() {
    # Create default config directory
    local config_dir="/etc/gatel"
    if [[ "$(detect_os)" == "macos" ]]; then
        config_dir="${PREFIX}/etc/gatel"
    fi

    if [[ -w "$(dirname "$config_dir")" ]] || [[ -d "$config_dir" ]]; then
        mkdir -p "$config_dir" 2>/dev/null || sudo mkdir -p "$config_dir"
    else
        sudo mkdir -p "$config_dir"
    fi

    # Write a default config if none exists
    if [[ ! -f "${config_dir}/gatel.kdl" ]]; then
        local writer="tee"
        if [[ ! -w "$config_dir" ]]; then
            writer="sudo tee"
        fi
        $writer "${config_dir}/gatel.kdl" >/dev/null <<'DEFAULTCONFIG'
global {
    log level="info"
    http ":80"
}

site "*" {
    route "/*" {
        respond "Hello from gatel!" status=200
    }
}
DEFAULTCONFIG
        info "Default config written to ${config_dir}/gatel.kdl"
    fi

    # Install systemd unit on Linux
    if [[ "$(detect_os)" == "linux" ]] && command -v systemctl &>/dev/null; then
        install_systemd_unit
    fi

    # Install launchd plist on macOS
    if [[ "$(detect_os)" == "macos" ]]; then
        install_launchd_plist
    fi
}

install_systemd_unit() {
    local unit_dir="/etc/systemd/system"
    local unit_file="${unit_dir}/gatel.service"

    if [[ -f "$unit_file" ]]; then
        return
    fi

    info "Installing systemd service unit"

    local writer="tee"
    if [[ ! -w "$unit_dir" ]]; then
        writer="sudo tee"
    fi

    $writer "$unit_file" >/dev/null <<EOF
[Unit]
Description=Gatel reverse proxy and web server
Documentation=https://github.com/${REPO}
After=network-online.target
Wants=network-online.target

[Service]
Type=notify
NotifyAccess=main
ExecStart=${PREFIX}/bin/gatel run --config /etc/gatel/gatel.kdl
ExecReload=/bin/kill -HUP \$MAINPID
Restart=on-failure
RestartSec=5
LimitNOFILE=1048576
AmbientCapabilities=CAP_NET_BIND_SERVICE
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/etc/gatel /var/log/gatel

[Install]
WantedBy=multi-user.target
EOF

    info "Systemd unit installed. Enable with:"
    info "  sudo systemctl daemon-reload"
    info "  sudo systemctl enable --now gatel"
}

install_launchd_plist() {
    local plist_dir="${HOME}/Library/LaunchAgents"
    local plist_file="${plist_dir}/com.gatel.server.plist"

    if [[ -f "$plist_file" ]]; then
        return
    fi

    mkdir -p "$plist_dir"

    info "Installing launchd plist"

    cat > "$plist_file" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.gatel.server</string>
    <key>ProgramArguments</key>
    <array>
        <string>${PREFIX}/bin/gatel</string>
        <string>run</string>
        <string>--config</string>
        <string>${PREFIX}/etc/gatel/gatel.kdl</string>
    </array>
    <key>RunAtLoad</key>
    <false/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>StandardErrorPath</key>
    <string>/tmp/gatel.err.log</string>
    <key>StandardOutPath</key>
    <string>/tmp/gatel.out.log</string>
</dict>
</plist>
EOF

    info "Launchd plist installed. Start with:"
    info "  launchctl load ${plist_file}"
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

if $FROM_SOURCE; then
    install_from_source
else
    install_from_binary
fi

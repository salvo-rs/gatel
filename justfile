prefix     := "/usr/local"
bindir     := prefix / "bin"
configdir  := "/etc/gatel"
cargo      := env("CARGO", "cargo")
target     := env("TARGET", "")
version    := `grep '^version' crates/gatel/Cargo.toml | head -1 | cut -d'"' -f2`

bins := "gatel gatel-passwd gatel-precompress"

# Cross-compilation support
cargo_build := if target != "" { cargo + " build --release --target " + target } else { cargo + " build --release" }
bin_dir     := if target != "" { "target/" + target + "/release" } else { "target/release" }

# List available recipes
default:
    @just --list

# Configure git hooks
setup:
    git config core.hooksPath .githooks
    @echo "Git hooks configured."

# Build release binaries
build:
    {{ cargo_build }}

# Build debug binaries
build-dev:
    {{ cargo }} build

# Build and run with default config
run: build-dev
    {{ cargo }} run -- run --config gatel.kdl

# Run all tests
test:
    {{ cargo }} test --workspace

# Run clippy lints
lint:
    {{ cargo }} clippy --workspace --all-targets -- -D warnings

# Check formatting
fmt:
    cargo +nightly fmt --all -- --check

# Run fmt + lint + test
check: fmt lint test

# Install binaries and config to system
install dest="": build
    #!/usr/bin/env bash
    set -euo pipefail
    dest="{{ dest }}"
    bindir="${dest}{{ bindir }}"
    configdir="${dest}{{ configdir }}"
    install -d "$bindir"
    for bin in {{ bins }}; do
        if [ -f "{{ bin_dir }}/$bin" ]; then
            install -m 755 "{{ bin_dir }}/$bin" "$bindir/$bin"
        fi
    done
    echo ""
    echo "Installed to $bindir"
    install -d "$configdir"
    if [ ! -f "$configdir/gatel.kdl" ]; then
        install -m 644 gatel.kdl "$configdir/gatel.kdl"
        echo "Default config installed to $configdir/gatel.kdl"
    fi

# Remove installed binaries
uninstall dest="":
    #!/usr/bin/env bash
    set -euo pipefail
    dest="{{ dest }}"
    bindir="${dest}{{ bindir }}"
    configdir="${dest}{{ configdir }}"
    for bin in {{ bins }}; do
        rm -f "$bindir/$bin"
    done
    echo "Binaries removed from $bindir"
    echo "Config in $configdir was preserved."

# Remove build artifacts
clean:
    {{ cargo }} clean

# Build Docker image (distroless)
docker:
    docker build -t gatel:latest -t gatel:{{ version }} .

# Build Docker image (Alpine)
docker-alpine:
    docker build -f Dockerfile.alpine -t gatel:alpine -t gatel:{{ version }}-alpine .

# Create release archive
package: build
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p dist
    cd "{{ bin_dir }}"
    tar czf "{{ justfile_directory() }}/dist/gatel-{{ version }}.tar.gz" {{ bins }}
    echo "==> dist/gatel-{{ version }}.tar.gz"

# Build Debian package
package-deb: build
    bash packaging/deb/build.sh {{ bin_dir }}/gatel v{{ version }}

# Build RPM package
package-rpm: build
    bash packaging/rpm/build.sh {{ bin_dir }}/gatel v{{ version }}

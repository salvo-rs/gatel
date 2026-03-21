PREFIX     ?= /usr/local
BINDIR     ?= $(PREFIX)/bin
CONFIGDIR  ?= /etc/gatel
CARGO      ?= cargo
INSTALL    ?= install
VERSION    ?= $(shell grep '^version' crates/gatel/Cargo.toml | head -1 | cut -d'"' -f2)
TARGET     ?=

BINS = gatel gatel-passwd gatel-precompress

# Cross-compilation support
ifdef TARGET
  CARGO_BUILD = $(CARGO) build --release --target $(TARGET)
  BIN_DIR = target/$(TARGET)/release
else
  CARGO_BUILD = $(CARGO) build --release
  BIN_DIR = target/release
endif

.PHONY: all build build-dev run test lint fmt check install uninstall clean \
        docker docker-alpine package package-deb package-rpm help

all: build

help:
	@echo "Gatel build targets:"
	@echo ""
	@echo "  build          Build release binaries"
	@echo "  build-dev      Build debug binaries"
	@echo "  run            Build and run with default config"
	@echo "  test           Run all tests"
	@echo "  lint           Run clippy lints"
	@echo "  fmt            Check formatting"
	@echo "  check          Run fmt + lint + test"
	@echo "  install        Install binaries and config to system"
	@echo "  uninstall      Remove installed binaries"
	@echo "  clean          Remove build artifacts"
	@echo "  docker         Build Docker image (distroless)"
	@echo "  docker-alpine  Build Docker image (Alpine)"
	@echo "  package        Create release archive"
	@echo "  package-deb    Build Debian package"
	@echo "  package-rpm    Build RPM package"
	@echo ""
	@echo "Variables:"
	@echo "  PREFIX=$(PREFIX)  CONFIGDIR=$(CONFIGDIR)"
	@echo "  TARGET=$(TARGET)  (set for cross-compilation)"

build:
	$(CARGO_BUILD)

build-dev:
	$(CARGO) build

run: build-dev
	$(CARGO) run -- run --config gatel.kdl

test:
	$(CARGO) test --workspace

lint:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

fmt:
	cargo +nightly fmt --all -- --check

check: fmt lint test

install: build
	$(INSTALL) -d $(DESTDIR)$(BINDIR)
	$(foreach bin,$(BINS),\
		$(if $(wildcard $(BIN_DIR)/$(bin)),\
			$(INSTALL) -m 755 $(BIN_DIR)/$(bin) $(DESTDIR)$(BINDIR)/$(bin);))
	@echo ""
	@echo "Installed to $(DESTDIR)$(BINDIR)"
	$(INSTALL) -d $(DESTDIR)$(CONFIGDIR)
	@if [ ! -f "$(DESTDIR)$(CONFIGDIR)/gatel.kdl" ]; then \
		$(INSTALL) -m 644 gatel.kdl $(DESTDIR)$(CONFIGDIR)/gatel.kdl; \
		echo "Default config installed to $(DESTDIR)$(CONFIGDIR)/gatel.kdl"; \
	fi

uninstall:
	$(foreach bin,$(BINS),rm -f $(DESTDIR)$(BINDIR)/$(bin);)
	@echo "Binaries removed from $(DESTDIR)$(BINDIR)"
	@echo "Config in $(DESTDIR)$(CONFIGDIR) was preserved."

clean:
	$(CARGO) clean

# Docker
docker:
	docker build -t gatel:latest -t gatel:$(VERSION) .

docker-alpine:
	docker build -f Dockerfile.alpine -t gatel:alpine -t gatel:$(VERSION)-alpine .

# Packaging
package: build
	@mkdir -p dist
	@cd $(BIN_DIR) && tar czf $(CURDIR)/dist/gatel-$(VERSION).tar.gz \
		$(foreach bin,$(BINS),$(bin))
	@echo "==> dist/gatel-$(VERSION).tar.gz"

package-deb: build
	bash packaging/deb/build.sh $(BIN_DIR)/gatel v$(VERSION)

package-rpm: build
	bash packaging/rpm/build.sh $(BIN_DIR)/gatel v$(VERSION)

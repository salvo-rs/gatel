ARG BUILDER=source-build

# ---- Build stage ----
FROM rust:1-bookworm AS source-build

WORKDIR /build

# Copy manifests first for layer caching
COPY Cargo.toml Cargo.lock ./
COPY crates/core/Cargo.toml crates/core/Cargo.toml
COPY crates/gatel/Cargo.toml crates/gatel/Cargo.toml
COPY crates/passwd/Cargo.toml crates/passwd/Cargo.toml
COPY crates/precompress/Cargo.toml crates/precompress/Cargo.toml

# Stub out src dirs so cargo can resolve the workspace
RUN mkdir -p crates/core/src crates/gatel/src crates/passwd/src crates/precompress/src \
    && echo "fn main(){}" > crates/gatel/src/main.rs \
    && echo "fn main(){}" > crates/passwd/src/main.rs \
    && echo "fn main(){}" > crates/precompress/src/main.rs \
    && touch crates/core/src/lib.rs

# Pre-build dependencies (cached unless Cargo.toml changes)
RUN cargo build --release --workspace 2>/dev/null || true

# Copy real source
COPY . .

# Touch source files to invalidate the stub build
RUN touch crates/core/src/lib.rs crates/gatel/src/main.rs \
    crates/passwd/src/main.rs crates/precompress/src/main.rs

RUN cargo build --release --workspace \
    && mkdir /out \
    && cp target/release/gatel target/release/gatel-passwd target/release/gatel-precompress /out/

# ---- Pre-built binaries (CI release) ----
FROM scratch AS prebuilt
ARG TARGETPLATFORM
COPY staging/${TARGETPLATFORM}/ /out/

# ---- Select builder ----
FROM ${BUILDER} AS builder

# ---- Runtime stage (distroless) ----
FROM gcr.io/distroless/cc-debian12:nonroot

COPY --from=builder /out/gatel /usr/local/bin/gatel
COPY --from=builder /out/gatel-passwd /usr/local/bin/gatel-passwd
COPY --from=builder /out/gatel-precompress /usr/local/bin/gatel-precompress

# Default config directory
COPY gatel.kdl /etc/gatel/gatel.kdl

EXPOSE 80 443 443/udp 2019

ENTRYPOINT ["gatel"]
CMD ["run", "--config", "/etc/gatel/gatel.kdl"]

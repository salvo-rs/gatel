FROM rust:1-bookworm AS builder

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY crates/core/Cargo.toml crates/core/Cargo.toml
COPY crates/gatel/Cargo.toml crates/gatel/Cargo.toml
COPY crates/passwd/Cargo.toml crates/passwd/Cargo.toml
COPY crates/precompress/Cargo.toml crates/precompress/Cargo.toml

RUN mkdir -p crates/core/src crates/gatel/src crates/passwd/src crates/precompress/src \
    && echo "pub fn __bench_placeholder() {}" > crates/core/src/lib.rs \
    && echo "fn main() {}" > crates/gatel/src/main.rs \
    && echo "fn main() {}" > crates/passwd/src/main.rs \
    && echo "fn main() {}" > crates/precompress/src/main.rs

RUN cargo build --release -p gatel 2>/dev/null || true

COPY crates/core/src crates/core/src
COPY crates/gatel/src crates/gatel/src
COPY crates/passwd/src crates/passwd/src
COPY crates/precompress/src crates/precompress/src

RUN cargo clean --release -p gatel-core -p gatel \
    && cargo build --release -p gatel \
    && mkdir -p /out \
    && cp target/release/gatel /out/gatel

FROM debian:bookworm-slim

COPY --from=builder /out/gatel /usr/local/bin/gatel
COPY gatel.kdl /etc/gatel/gatel.kdl

EXPOSE 8080

ENTRYPOINT ["gatel"]
CMD ["run", "--config", "/etc/gatel/gatel.kdl"]

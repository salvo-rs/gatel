# Gatel as a container image.
#
# Two ways in, one result:
#
#   docker build .                          # builds from source
#   docker build --build-arg SOURCE=prebuilt \
#                --build-arg TARGETPLATFORM=linux/amd64 .
#
# The second is what the release workflow uses, so the image ships exactly the
# binary that was tested and checksummed rather than a second build of the same
# commit.
#
# The final stage is `scratch`. A statically linked musl binary needs nothing
# else, and an image with nothing in it has nothing an advisory can ever be
# filed against. It matters here for a second reason: musl resolves names with
# its own resolver rather than through NSS, which is what lets a static build
# reach a backend by name at all.

ARG SOURCE=build

# ---------------------------------------------------------------- from source
FROM rust:1-alpine AS build-source
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p gatel \
    && mkdir -p /out && cp target/release/gatel /out/gatel

# ------------------------------------------------------------ already built
# `context/<platform>/gatel`, laid out by the release workflow. BuildKit sets
# TARGETPLATFORM per architecture, so one build serves both.
FROM alpine:3 AS build-prebuilt
ARG TARGETPLATFORM
COPY context/${TARGETPLATFORM}/gatel /out/gatel
RUN chmod +x /out/gatel

# ------------------------------------------------------------------ the image
FROM build-${SOURCE} AS staged

FROM scratch
COPY --from=staged /out/gatel /usr/local/bin/gatel

# A configuration is mounted here; certificates and ACME state are written
# beside it, so one volume covers everything that has to survive a restart.
WORKDIR /etc/gatel

# Nothing here needs root, and a gateway is the last process that should have
# it. Ports below 1024 are reached by publishing them, or by granting
# NET_BIND_SERVICE — not by running as root.
USER 65532:65532

EXPOSE 8080 8443

# `validate` exits non-zero on a configuration the gateway would refuse, and
# binds nothing, so it is safe to run against a live container.
HEALTHCHECK --interval=30s --timeout=3s \
    CMD ["/usr/local/bin/gatel", "validate", "/etc/gatel/gatel.kdl"]

ENTRYPOINT ["/usr/local/bin/gatel"]
CMD ["run", "/etc/gatel/gatel.kdl"]

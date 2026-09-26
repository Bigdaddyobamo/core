# syntax=docker/dockerfile:1
# Base images are pinned by tag and digest so rebuilds are reproducible;
# bump them deliberately (e.g. via Dependabot's docker ecosystem).

# Builder stage: compile the txwatch binary
FROM rust:1.98.1-alpine3.24@sha256:7cc1c22d77d9432f7fe012a70e6d3e555af54c2a6832700ed7d553f1769ae89f AS builder
WORKDIR /build
# reqwest's default TLS is rustls with the aws-lc-rs crypto provider (no
# OpenSSL); its C sources need a C toolchain and cmake to build against musl.
RUN apk add --no-cache musl-dev build-base cmake perl
COPY . .
# --locked: build exactly the audited Cargo.lock that cargo-deny/cargo-audit check.
# The cache mounts keep downloaded crates and compiled dependencies between
# builds, so a source change only recompiles the workspace crates.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked -p txwatch && \
    cp target/release/txwatch /usr/local/bin/txwatch

# Runtime stage: minimal image with only the binary
FROM alpine:3.24.2@sha256:294b683cb724975bec92580e1e685676bd4b50bda910ddb8c51d4cabeaec77e6
# rustls-platform-verifier loads trust roots from the system CA store.
RUN apk add --no-cache ca-certificates && \
    addgroup -S -g 10001 txwatch && \
    adduser -S -D -H -u 10001 -G txwatch txwatch && \
    mkdir /data && chown txwatch:txwatch /data
COPY --from=builder /usr/local/bin/txwatch /usr/local/bin/txwatch
# Mount your config here, or point TXWATCH_CONFIG / --config elsewhere.
ENV TXWATCH_CONFIG=/config/txwatch.toml

# Run unprivileged. /data is the writable working directory: point
# cursor_file there (e.g. cursor_file = "/data/cursors.json") and mount the
# config read-only (e.g. -v ./txwatch.toml:/config/txwatch.toml:ro).
USER txwatch
WORKDIR /data

# No HEALTHCHECK baked in: /healthz is only served when the binary is built
# with `--features metrics` and run as `watch --metrics-addr ...`, which this
# default image doesn't do. For such an image, add e.g.:
#   HEALTHCHECK CMD wget -qO- http://127.0.0.1:9090/healthz || exit 1

ENTRYPOINT ["txwatch"]

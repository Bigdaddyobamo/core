# Builder stage: compile the txwatch binary
FROM rust:alpine AS builder
WORKDIR /build
# reqwest's default TLS is rustls with the aws-lc-rs crypto provider (no
# OpenSSL); its C sources need a C toolchain and cmake to build against musl.
RUN apk add --no-cache musl-dev build-base cmake perl
COPY . .
# --locked: build exactly the audited Cargo.lock that cargo-deny/cargo-audit check.
RUN cargo build --release --locked -p txwatch

# Runtime stage: minimal image with only the binary
FROM alpine:latest
# rustls-platform-verifier loads trust roots from the system CA store.
RUN apk add --no-cache ca-certificates
COPY --from=builder /build/target/release/txwatch /usr/local/bin/txwatch
# Mount your config here, or point TXWATCH_CONFIG / --config elsewhere.
ENV TXWATCH_CONFIG=/config/txwatch.toml
ENTRYPOINT ["txwatch"]

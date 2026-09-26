# Builder stage: compile the txwatch binary
FROM rust:alpine AS builder
WORKDIR /build
COPY . .
RUN apk add --no-cache musl-dev && \
    cargo build --release -p txwatch

# Runtime stage: minimal image with only the binary
FROM alpine:latest
COPY --from=builder /build/target/release/txwatch /usr/local/bin/txwatch
# Mount your config here, or point TXWATCH_CONFIG / --config elsewhere.
ENV TXWATCH_CONFIG=/config/txwatch.toml
ENTRYPOINT ["txwatch"]

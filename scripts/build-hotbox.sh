#!/usr/bin/env bash

### Cross-compile the hotbox binary for Linux using Docker.
### Used during local development on macOS/Windows where `cargo build`
### produces a native binary that cannot run inside Linux containers.
###
### Output: target/hotbox-linux-{arm64|x86_64}

set -e

cd "$(dirname "$0")/.."

if ! command -v docker > /dev/null 2>&1; then
    echo "Error: Docker is required to cross-compile hotbox for Linux"
    exit 1
fi

# Detect the Docker host architecture.
DOCKER_ARCH=$(docker info --format '{{.Architecture}}' 2>/dev/null || echo "unknown")
case "$DOCKER_ARCH" in
    aarch64|arm64)  ARCH="arm64" ;;
    x86_64|amd64)   ARCH="x86_64" ;;
    *)
        echo "Error: Unsupported Docker architecture: $DOCKER_ARCH"
        exit 1
        ;;
esac

OUTPUT="target/hotbox-linux-${ARCH}"
echo "Building hotbox for linux/${ARCH}..."

CARGO_LOCK_HASH=$(shasum -a 256 Cargo.lock 2>/dev/null | cut -c1-16 || sha256sum Cargo.lock | cut -c1-16)

DOCKER_BUILDKIT=1 docker build --progress=plain \
    --build-arg CARGO_LOCK_HASH="$CARGO_LOCK_HASH" \
    -f - \
    -t hotbox-builder:latest \
    --output "type=local,dest=target/hotbox-build" \
    . <<'DOCKERFILE'
# syntax=docker/dockerfile:1.4
FROM rust:1.95.0-slim AS builder

RUN apt-get update && apt-get install -y musl-tools && rm -rf /var/lib/apt/lists/*

# Detect native architecture and add the corresponding musl target.
RUN ARCH=$(uname -m) && \
    if [ "$ARCH" = "aarch64" ] || [ "$ARCH" = "arm64" ]; then \
      MUSL_TARGET="aarch64-unknown-linux-musl"; \
    else \
      MUSL_TARGET="x86_64-unknown-linux-musl"; \
    fi && \
    rustup target add $MUSL_TARGET && \
    echo "$MUSL_TARGET" > /tmp/musl-target

ENV CARGO_INCREMENTAL=1 \
    CARGO_NET_RETRY=2

WORKDIR /workspace

COPY Cargo.toml Cargo.lock ./
COPY crates/hotbox ./crates/hotbox/
COPY crates/ ./crates/

ARG CARGO_LOCK_HASH=default

RUN --mount=type=cache,id=hotbox-registry-${CARGO_LOCK_HASH},target=/usr/local/cargo/registry \
    --mount=type=cache,id=hotbox-git-${CARGO_LOCK_HASH},target=/usr/local/cargo/git \
    --mount=type=cache,id=hotbox-target-${CARGO_LOCK_HASH},target=/workspace/target \
    MUSL_TARGET=$(cat /tmp/musl-target) && \
    cargo build --release --target $MUSL_TARGET -p hotbox && \
    cp /workspace/target/$MUSL_TARGET/release/hotbox /hotbox

FROM scratch
COPY --from=builder /hotbox /hotbox
DOCKERFILE

mv target/hotbox-build/hotbox "$OUTPUT"
chmod +x "$OUTPUT"
rm -rf target/hotbox-build

echo "Built: $OUTPUT"
file "$OUTPUT"

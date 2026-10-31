# Multi-stage build for Windows targets using cross-compilation
# Pin the Debian release (12, bookworm) so the builder's glibc does not
# change when the bare rust:1.99.1 tag moves to a newer Debian.
FROM rust:1.99.1-bookworm AS builder

ARG TAILWIND_VERSION=v4.3.3

# Download the Tailwind CSS standalone executable for the container
# architecture, verified against the SHA-256 the script pins per asset.
COPY scripts/install-tailwind.sh /tmp/install-tailwind.sh
RUN bash /tmp/install-tailwind.sh "${TAILWIND_VERSION}" /usr/local/bin && rm /tmp/install-tailwind.sh

# Install mingw-w64 and build dependencies for Windows cross-compilation
RUN apt-get update && apt-get install -y \
    gcc-mingw-w64-x86-64 \
    gcc-mingw-w64-i686 \
    cmake \
    nasm \
    && rm -rf /var/lib/apt/lists/*

# Add Windows target (64-bit only for simplicity)
RUN rustup target add x86_64-pc-windows-gnu

# Configure cargo for cross-compilation
RUN mkdir -p /root/.cargo && echo '[target.x86_64-pc-windows-gnu]\nlinker = "x86_64-w64-mingw32-gcc"' > /root/.cargo/config.toml

# Set up the workspace
WORKDIR /workspace

# Copy source code
COPY . .

# Build for Windows (64-bit)
RUN cargo build --release --locked --target x86_64-pc-windows-gnu

# The builder stage contains our binaries - we'll extract them with docker cp

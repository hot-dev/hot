# Multi-stage build for Linux targets
FROM rust:1.99.1 as builder

ARG TAILWIND_VERSION=v4.3.3

# Download the Tailwind CSS standalone executable for the container
# architecture, verified against the SHA-256 the script pins per asset.
COPY scripts/install-tailwind.sh /tmp/install-tailwind.sh
RUN bash /tmp/install-tailwind.sh "${TAILWIND_VERSION}" /usr/local/bin && rm /tmp/install-tailwind.sh

# Install necessary packages for cross-compilation
RUN apt-get update && apt-get install -y \
    gcc-aarch64-linux-gnu \
    gcc-x86-64-linux-gnu \
    g++-aarch64-linux-gnu \
    g++-x86-64-linux-gnu \
    libc6-dev-arm64-cross \
    libc6-dev-amd64-cross \
    && rm -rf /var/lib/apt/lists/*

# Add the target architectures
RUN rustup target add aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu

# Configure cross-compilation environment variables and linkers
ENV CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc \
    AR_x86_64_unknown_linux_gnu=x86_64-linux-gnu-ar \
    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
    AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc

# Set up the workspace
WORKDIR /workspace

# Copy source code
COPY . .

# Build for both Linux architectures
RUN cargo build --release --target x86_64-unknown-linux-gnu
RUN cargo build --release --target aarch64-unknown-linux-gnu

# The builder stage contains our binaries - we'll extract them with docker cp

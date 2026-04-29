# Multi-stage build for Linux targets
FROM rustlang/rust:nightly as builder

# Download Tailwind CSS standalone executable (for the container architecture)
RUN ARCH=$(uname -m) && \
    if [ "$ARCH" = "x86_64" ]; then \
        curl -sLO https://github.com/tailwindlabs/tailwindcss/releases/latest/download/tailwindcss-linux-x64 && \
        chmod +x tailwindcss-linux-x64 && \
        mv tailwindcss-linux-x64 /usr/local/bin/tailwindcss; \
    elif [ "$ARCH" = "aarch64" ]; then \
        curl -sLO https://github.com/tailwindlabs/tailwindcss/releases/latest/download/tailwindcss-linux-arm64 && \
        chmod +x tailwindcss-linux-arm64 && \
        mv tailwindcss-linux-arm64 /usr/local/bin/tailwindcss; \
    else \
        echo "Unsupported architecture: $ARCH" && exit 1; \
    fi

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
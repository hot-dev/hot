# Multi-stage build for Windows targets using cross-compilation
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
RUN cargo build --release --target x86_64-pc-windows-gnu

# The builder stage contains our binaries - we'll extract them with docker cp
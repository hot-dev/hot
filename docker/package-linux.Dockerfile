FROM ubuntu:24.04

# Install packaging tools and essential runtime libraries
RUN apt-get update && apt-get install -y \
    curl \
    build-essential \
    dpkg-dev \
    rpm \
    libc6-dev \
    libc6 \
    libgcc-s1 \
    libssl-dev \
    libssl3t64 \
    pkg-config \
    file \
    binutils \
    && rm -rf /var/lib/apt/lists/*

# Install Rust 1.99.1 with a pinned rustup-init, checked against the
# SHA-256 that static.rust-lang.org publishes for rustup 1.29.2. To move to a
# new rustup, update the version and both digests from
# https://static.rust-lang.org/rustup/archive/<version>/<target>/rustup-init.sha256
RUN set -eux; \
    case "$(uname -m)" in \
        x86_64) target=x86_64-unknown-linux-gnu; \
                sha256=5be191e8c70a5007074254cb74d78dbfdd25cd5a61c27de7b8e5410122ec24f4 ;; \
        aarch64) target=aarch64-unknown-linux-gnu; \
                 sha256=a9c168dbea81a84671c6b7a5c1795cb0eb699020110d107ddfca427bd797e697 ;; \
        *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;; \
    esac; \
    curl --proto '=https' --tlsv1.2 -fsSL -o /tmp/rustup-init \
        "https://static.rust-lang.org/rustup/archive/1.29.2/${target}/rustup-init"; \
    echo "${sha256}  /tmp/rustup-init" | sha256sum -c -; \
    chmod +x /tmp/rustup-init; \
    /tmp/rustup-init -y --no-modify-path --profile minimal --default-toolchain 1.99.1; \
    rm /tmp/rustup-init
ENV PATH="/root/.cargo/bin:${PATH}"

# Install pinned packaging tools separately to avoid dependency conflicts
RUN cargo install cargo-deb --version 3.8.0 --locked
RUN cargo install cargo-generate-rpm --version 0.21.1 --locked

WORKDIR /workspace

# Copy the entire project
COPY . .

# Copy and setup the packaging script
COPY docker/package-inside-docker.sh .
RUN chmod +x package-inside-docker.sh

CMD ["./package-inside-docker.sh"]

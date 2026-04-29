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

# Install Rust (latest stable)
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"

# Install packaging tools separately to avoid dependency conflicts
RUN cargo install cargo-deb --locked
RUN cargo install cargo-generate-rpm --locked

WORKDIR /workspace

# Copy the entire project
COPY . .

# Copy and setup the packaging script
COPY docker/package-inside-docker.sh .
RUN chmod +x package-inside-docker.sh

CMD ["./package-inside-docker.sh"]

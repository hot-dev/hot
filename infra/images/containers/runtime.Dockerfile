# syntax=docker/dockerfile:1.7
#
# The Hot self-host runtime image. One linux/amd64 image runs every service
# and one-shot job with a different command: `app`, `api`, `worker`,
# `scheduler`, `task-worker`, `db migrate`, and `admin bootstrap`.
#
# Build it from a clean public checkout with infra/images/build-runtime.sh,
# which records the git revision first. The layout matches the Linux
# package: `/usr/bin/hot` finds its resources in `/usr/share/hot`.

ARG DEBIAN_IMAGE=debian:bookworm-slim@sha256:99c0b25c46d0214d5a2091092e7908907dcaecd6a962cde0ac700acee222b6f3

FROM --platform=linux/amd64 ${DEBIAN_IMAGE} AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        build-essential \
        protobuf-compiler \
        musl-tools \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

ARG RUSTUP_VERSION=1.28.2
ARG RUSTUP_SHA256=20a06e644b0d9bd2fbdbfd52d42540bdde820ea7df86e92e533c073da0cdd43c
ARG RUST_VERSION=1.99.1
RUN curl --proto '=https' --tlsv1.2 -fsSL -o /tmp/rustup-init \
        "https://static.rust-lang.org/rustup/archive/${RUSTUP_VERSION}/x86_64-unknown-linux-gnu/rustup-init" \
    && echo "${RUSTUP_SHA256}  /tmp/rustup-init" | sha256sum --check - \
    && chmod +x /tmp/rustup-init \
    && /tmp/rustup-init -y --profile minimal --default-toolchain "${RUST_VERSION}" \
        --target x86_64-unknown-linux-musl \
    && rm -f /tmp/rustup-init
ENV PATH="/root/.cargo/bin:${PATH}"

# The app's stylesheet is generated at build time; CI pins the same release.
ARG TAILWIND_VERSION=v4.3.0
ARG TAILWIND_SHA256=73f0e5459054e5cfaa8ab6f3b940f3fbe0f13cc7fd83bc24e7c655033c203400
RUN curl -fsSL -o /usr/local/bin/tailwindcss \
        "https://github.com/tailwindlabs/tailwindcss/releases/download/${TAILWIND_VERSION}/tailwindcss-linux-x64" \
    && echo "${TAILWIND_SHA256}  /usr/local/bin/tailwindcss" | sha256sum --check - \
    && chmod +x /usr/local/bin/tailwindcss

# `ctr` imports box images into the Kata containerd on the host.
ARG CONTAINERD_VERSION=1.7.30
ARG CONTAINERD_SHA256=ca0f27e34411504acd1cd24fcbaa71b9c47d31ce9408c47c54a2dc1810ceb1df
RUN curl -fsSL -o /tmp/containerd.tar.gz \
        "https://github.com/containerd/containerd/releases/download/v${CONTAINERD_VERSION}/containerd-${CONTAINERD_VERSION}-linux-amd64.tar.gz" \
    && echo "${CONTAINERD_SHA256}  /tmp/containerd.tar.gz" | sha256sum --check - \
    && mkdir -p /out \
    && tar -xzf /tmp/containerd.tar.gz -C /tmp bin/ctr \
    && mv /tmp/bin/ctr /out/ctr \
    && rm -rf /tmp/containerd.tar.gz /tmp/bin

ENV CARGO_INCREMENTAL=0 \
    CARGO_NET_RETRY=2 \
    CARGO_TERM_COLOR=never

WORKDIR /src
COPY . .

RUN test -s resources/git-revision.txt \
    || { echo "run scripts/capture-git-revision.sh before building" >&2; exit 1; }

# S3 and GCS storage are always compiled into `hot`; Kata is opt-in.
RUN --mount=type=cache,id=hot-runtime-registry,target=/root/.cargo/registry \
    --mount=type=cache,id=hot-runtime-target,target=/src/target \
    cargo build --release --locked -p hot_cli --features kata \
    && cargo build --release --locked --target x86_64-unknown-linux-musl -p hotbox \
    && cp target/release/hot /out/hot \
    && cp target/x86_64-unknown-linux-musl/release/hotbox /out/hotbox-linux-x86_64

FROM --platform=linux/amd64 ${DEBIAN_IMAGE}

# iproute2 and iptables: the task worker manages box network namespaces.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        iproute2 \
        iptables \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 2000 kata-containerd \
    && groupadd --gid 10001 hot \
    && useradd --uid 10001 --gid 10001 --home-dir /var/lib/hot --no-create-home \
        --shell /usr/sbin/nologin hot \
    && mkdir -p /var/lib/hot \
    && chown hot:hot /var/lib/hot

COPY --from=builder /out/hot /usr/bin/hot
COPY --from=builder /out/ctr /usr/local/bin/ctr
COPY --from=builder /out/hotbox-linux-x86_64 /usr/share/hot/bin/hotbox-linux-x86_64
COPY --from=builder /src/resources/db /usr/share/hot/db
COPY --from=builder /src/resources/app /usr/share/hot/app
COPY --from=builder /src/resources/ai /usr/share/hot/ai
COPY --from=builder /src/resources/init /usr/share/hot/init
COPY --from=builder /src/hot/pkg/hot-std /usr/share/hot/pkg/hot-std
COPY --from=builder /src/LICENSE /src/NOTICE /usr/share/hot/

LABEL org.opencontainers.image.title="Hot runtime" \
      org.opencontainers.image.source="https://github.com/hot-dev/hot" \
      org.opencontainers.image.licenses="Apache-2.0"

# Services run as `hot`; the task worker overrides this with 0:2000 because
# it drives the host's Kata containerd and network namespaces.
USER 10001:10001
WORKDIR /var/lib/hot
ENTRYPOINT ["/usr/bin/hot"]
CMD ["--version"]

# ── Stage 1: Builder ──────────────────────────────────────────────────────────
FROM rust:1.88-slim AS builder

RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

ENV OPENSSL_NO_VENDOR=1

WORKDIR /build

# Workspace manifest includes all three crates so both binaries are built here.
RUN printf '[workspace]\nmembers = ["core", "lair", "server", "k8s-ops"]\nresolver = "2"\n' > Cargo.toml

COPY Cargo.lock ./
COPY core/Cargo.toml    core/
COPY lair/Cargo.toml  lair/
COPY server/Cargo.toml  server/
COPY k8s-ops/Cargo.toml k8s-ops/

# Stub sources for dep-caching layer.
RUN mkdir -p core/src lair/src server/src k8s-ops/src && \
    echo '' > core/src/lib.rs && \
    echo 'fn main() {}' > lair/src/main.rs && \
    echo 'fn main() {}' > server/src/main.rs && \
    echo '' > k8s-ops/src/lib.rs

RUN cargo fetch

COPY core/src    core/src
COPY lair/src  lair/src
COPY server/src  server/src
COPY k8s-ops/src k8s-ops/src

RUN cargo build --release -p octo-lair -p octo-server

# ── Stage 2: Runtime ──────────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates curl git gpg && \
    curl -fsSL https://deb.nodesource.com/setup_22.x | bash - && \
    curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
        | gpg --dearmor -o /usr/share/keyrings/githubcli-archive-keyring.gpg && \
    echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
        > /etc/apt/sources.list.d/github-cli.list && \
    apt-get update && \
    apt-get install -y \
    awscli \
    gh \
    jq \
    nodejs \
    qrencode \
    && rm -rf /var/lib/apt/lists/* && \
    # uvx (via uv)
    curl -LsSf https://astral.sh/uv/install.sh | UV_INSTALL_DIR=/usr/local/bin sh

COPY --from=builder /build/target/release/octo-lair /usr/local/bin/octo-lair
COPY --from=builder /build/target/release/octo-server /usr/local/bin/octo-server

COPY lair/docker-entrypoint.sh        /usr/local/bin/docker-entrypoint.sh
COPY server/docker-entrypoint.sh        /usr/local/bin/docker-entrypoint-server.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh /usr/local/bin/docker-entrypoint-server.sh

LABEL org.opencontainers.image.source=https://github.com/georgebradford0/octo

ENV HOME=/root
ENV OCTO_SKIP_SHELL_ENV=1
ENV OCTO_DATA_DIR=/data
ENV NOISE_KEY_FILE=/data/noise_key.bin

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]

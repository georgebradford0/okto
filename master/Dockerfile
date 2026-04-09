# ── Stage 1: Builder ──────────────────────────────────────────────────────────
FROM rust:1.85-slim AS builder

RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

ENV OPENSSL_NO_VENDOR=1

WORKDIR /build

# Workspace manifest that includes only the crates we need.
RUN printf '[workspace]\nmembers = ["core", "master"]\nresolver = "2"\n' > Cargo.toml

COPY Cargo.lock ./
COPY core/Cargo.toml   core/
COPY master/Cargo.toml master/

# Stub sources for dep-caching layer.
RUN mkdir -p core/src master/src && \
    echo '' > core/src/lib.rs && \
    echo 'fn main() {}' > master/src/main.rs

RUN cargo fetch

COPY core/src   core/src
COPY master/src master/src

RUN cargo build --release -p claudulhu-master

# ── Stage 2: Runtime ──────────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates curl gnupg && \
    install -m 0755 -d /etc/apt/keyrings && \
    curl -fsSL https://download.docker.com/linux/debian/gpg | gpg --dearmor -o /etc/apt/keyrings/docker.gpg && \
    chmod a+r /etc/apt/keyrings/docker.gpg && \
    echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.gpg] https://download.docker.com/linux/debian bookworm stable" \
      > /etc/apt/sources.list.d/docker.list && \
    apt-get update && apt-get install -y \
    docker-ce-cli \
    qrencode \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/claudulhu-master /usr/local/bin/claudulhu-master

COPY master/docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

ENV HOME=/root
ENV CLAUDULHU_SKIP_SHELL_ENV=1
ENV CLAUDULHU_DATA_DIR=/data
ENV NOISE_KEY_FILE=/data/noise_key.bin

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]

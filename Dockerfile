# ── Stage 1: Builder ──────────────────────────────────────────────────────────
FROM rust:1.85-slim AS builder

RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

ENV OPENSSL_NO_VENDOR=1

WORKDIR /build

# Write a server-only workspace manifest (desktop is never included)
RUN printf '[workspace]\nmembers = ["core", "server"]\nresolver = "2"\n' > Cargo.toml

COPY Cargo.lock ./
COPY core/Cargo.toml core/
COPY server/Cargo.toml server/

# Stub sources for dep-caching layer
RUN mkdir -p core/src server/src && \
    echo '' > core/src/lib.rs && \
    echo 'fn main() {}' > server/src/main.rs

RUN cargo fetch

# Now copy real source and build
COPY core/src  core/src
COPY server/src server/src

RUN cargo build --release -p claudulhu-server

# ── Stage 2: Runtime ──────────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    git \
    openssh-server \
    qrencode \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/claudulhu-server /usr/local/bin/claudulhu-server

COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

ENV HOME=/root
ENV CLAUDULHU_SKIP_SHELL_ENV=1

# 8000: claudulhu WebSocket/HTTP  22: SSH tunnel endpoint
EXPOSE 8000 2222

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]

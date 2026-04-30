# ── Stage 1: Builder ──────────────────────────────────────────────────────────
FROM rust:1.88-slim AS builder

RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

ENV OPENSSL_NO_VENDOR=1

WORKDIR /build

# Workspace manifest includes all three crates so both binaries are built here.
RUN printf '[workspace]\nmembers = ["core", "rulyeh", "server"]\nresolver = "2"\n' > Cargo.toml

COPY Cargo.lock ./
COPY core/Cargo.toml   core/
COPY rulyeh/Cargo.toml rulyeh/
COPY server/Cargo.toml server/

# Stub sources for dep-caching layer.
RUN mkdir -p core/src rulyeh/src server/src && \
    echo '' > core/src/lib.rs && \
    echo 'fn main() {}' > rulyeh/src/main.rs && \
    echo 'fn main() {}' > server/src/main.rs

RUN cargo fetch

COPY core/src   core/src
COPY rulyeh/src rulyeh/src
COPY server/src server/src

RUN cargo build --release -p claudulhu-rulyeh -p claudulhu-server

# ── Stage 2: Runtime ──────────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates curl git gnupg && \
    install -m 0755 -d /etc/apt/keyrings && \
    curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg | gpg --dearmor -o /etc/apt/keyrings/githubcli.gpg && \
    chmod go+r /etc/apt/keyrings/githubcli.gpg && \
    echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/githubcli.gpg] https://cli.github.com/packages stable main" \
      > /etc/apt/sources.list.d/github-cli.list && \
    apt-get update && apt-get install -y \
    awscli \
    gh \
    qrencode \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/claudulhu-rulyeh /usr/local/bin/claudulhu-rulyeh
COPY --from=builder /build/target/release/claudulhu-server /usr/local/bin/claudulhu-server

COPY rulyeh/docker-entrypoint.sh        /usr/local/bin/docker-entrypoint.sh
COPY server/docker-entrypoint.sh        /usr/local/bin/docker-entrypoint-server.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh /usr/local/bin/docker-entrypoint-server.sh

LABEL org.opencontainers.image.source=https://github.com/georgebradford0/claudulhu

ENV HOME=/root
ENV CLAUDULHU_SKIP_SHELL_ENV=1
ENV CLAUDULHU_DATA_DIR=/data
ENV NOISE_KEY_FILE=/data/noise_key.bin

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]

# ── Stage 1: Builder ──────────────────────────────────────────────────────────
FROM rust:1.88-slim AS builder

RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

ENV OPENSSL_NO_VENDOR=1

WORKDIR /build

# Workspace manifest includes the crates needed to build the merged lair binary.
# tests/ and cli/ are not built inside the image — they're for local use only.
RUN printf '[workspace]\nmembers = ["core", "lair"]\nresolver = "2"\n' > Cargo.toml

COPY Cargo.lock ./
COPY core/Cargo.toml    core/
COPY lair/Cargo.toml    lair/

# Stub sources for dep-caching layer.
RUN mkdir -p core/src lair/src && \
    echo '' > core/src/lib.rs && \
    echo 'fn main() {}' > lair/src/main.rs

# QEMU-emulated arm64 builds frequently lose crates.io HTTPS connections
# mid-`cargo fetch` (SSL_ERROR_SYSCALL). Crank cargo's own retry knobs and
# wrap the fetch in a shell retry — each attempt resumes from the cached
# crates the previous run did manage to download, so it converges fast.
ENV CARGO_NET_RETRY=10 \
    CARGO_HTTP_TIMEOUT=120 \
    CARGO_HTTP_MULTIPLEXING=false

RUN i=0; until cargo fetch; do \
        i=$((i+1)); \
        if [ "$i" -ge 5 ]; then echo "[cargo fetch] giving up after 5 attempts" >&2; exit 1; fi; \
        echo "[cargo fetch] attempt $i failed; retrying in 10s..." >&2; \
        sleep 10; \
    done

COPY core/src    core/src
COPY lair/src    lair/src

RUN cargo build --release -p octo-lair

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
    openssh-client \
    qrencode \
    && rm -rf /var/lib/apt/lists/* && \
    # uvx (via uv)
    curl -LsSf https://astral.sh/uv/install.sh | UV_INSTALL_DIR=/usr/local/bin sh

COPY --from=builder /build/target/release/octo-lair /usr/local/bin/octo-lair

LABEL org.opencontainers.image.source=https://github.com/georgebradford0/octo

ENV HOME=/root
ENV OCTO_SKIP_SHELL_ENV=1
ENV OCTO_DATA_DIR=/data
ENV NOISE_KEY_FILE=/data/noise_key.bin

# Default ENTRYPOINT runs the parent (lair) role. Child containers run the
# same image with `command: ["/usr/local/bin/octo-lair", "--role", "agent"]`
# (lair sets this when it `docker create`s an agent container).
ENTRYPOINT ["/usr/local/bin/octo-lair", "--role", "lair"]

# Multi-stage image for the lair binary. Built with `docker buildx` for
# linux/amd64 and linux/arm64; pushed to ghcr.io/georgebradford0/lair by the
# `lair.yml` GHA workflow (`gh workflow run lair.yml --ref main`) after a
# lair version bump. `scripts/build-lair-image.sh` is the local fallback.
#
# Runtime contract:
#   * Operator's host dir is bind-mounted at /data (config.json, lair/,
#     agents/, all live there).
#   * NOISE_PORT (default 8443) is published from the container; the loopback
#     HTTP management port (8000) is published to 127.0.0.1 only.
#   * Env vars passed via `docker run -e KEY=VAL` reach lair *and* every child
#     agent process it spawns — children inherit the lair process env via
#     tokio's Command default, no extra plumbing required.
#
# ── Stage 1: Builder ──────────────────────────────────────────────────────────
FROM rust:1.88-slim AS builder

RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

ENV OPENSSL_NO_VENDOR=1

WORKDIR /build

# Strip the workspace to just the crates needed for the lair binary. tests/
# and cli/ aren't built here — the CLI runs on the operator's host, not in
# the image.
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

RUN cargo build --release -p lair

# ── Stage 2: Runtime ──────────────────────────────────────────────────────────
FROM debian:bookworm-slim

# Tools lair's bash / git / mcp paths shell out to. `gh` and `node` are pulled
# in because most MCP servers in the wild are npx- or `gh`-driven; `uv`
# (provides `uvx`) covers the Python ecosystem.
RUN apt-get update && apt-get install -y ca-certificates curl git gpg && \
    curl -fsSL https://deb.nodesource.com/setup_22.x | bash - && \
    curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
        | gpg --dearmor -o /usr/share/keyrings/githubcli-archive-keyring.gpg && \
    echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
        > /etc/apt/sources.list.d/github-cli.list && \
    apt-get update && \
    apt-get install -y \
        gcc \
        gh \
        jq \
        nodejs \
        openssh-client \
        openssl \
        qrencode \
    && rm -rf /var/lib/apt/lists/* && \
    curl -LsSf https://astral.sh/uv/install.sh | UV_INSTALL_DIR=/usr/local/bin sh

COPY --from=builder /build/target/release/lair /usr/local/bin/lair

# Kaniko executor for building container images without a Docker daemon.
# Extracted from the official kaniko image via multi-stage COPY; agents invoke
# it via their bash tool with `--force` (since we're not inside the kaniko image).
# The /kaniko directory holds the executor binary, credential helpers, and SSL
# certs that Kaniko expects at runtime.
COPY --from=gcr.io/kaniko-project/executor:v1.24.0 /kaniko/ /kaniko/
# Ensure the executor is on PATH and runnable by non-root agent uids.
RUN ln -sf /kaniko/executor /usr/local/bin/kaniko && \
    chmod -R a+rX /kaniko

# Kaniko ships /kaniko/ssl/certs/ as a single PEM bundle, and the image sets
# SSL_CERT_DIR=/kaniko/ssl/certs globally (below). Go's TLS (Kaniko) reads
# bundles from a dir fine, but OpenSSL's `capath` mode requires one cert per
# file plus c_rehash-style hashed symlinks — without them, every Python /
# Ruby / curl-with-openssl client in the container fails TLS verification
# with "unable to get local issuer certificate" (e.g. mcp-proxy-for-aws
# surfaced this as a JSON-RPC -32602 during MCP initialize). Split the
# bundle into per-cert files and rehash so the directory works for both
# stacks. The original ca-certificates.crt is left intact for Kaniko.
RUN awk '/-----BEGIN CERTIFICATE-----/{n++} n{print > sprintf("/kaniko/ssl/certs/cert-%04d.pem", n)}' \
        /kaniko/ssl/certs/ca-certificates.crt && \
    c_rehash /kaniko/ssl/certs >/dev/null && \
    chmod -R a+rX /kaniko/ssl/certs

# Dedicated non-root users for child agent processes. Lair (the parent)
# keeps running as root inside the container so it can chown the bind-mounted
# /data/agents/<name>/ dirs after creating them. Each agent gets its own uid
# in the 10100..10199 range (one per available loopback port 30100..30199),
# which closes four vectors at once:
#   * `kill 1` against lair (different uid → EPERM)
#   * reading `LAIR_MGMT_TOKEN` from `/proc/1/environ` (different uid → EACCES)
#   * reading `/data/config.json` and `/data/lair/*` if those are 0600 root-owned
#   * sibling impersonation — agent A can't read agent B's `OKTO_AGENT_TOKEN`
#     from `/proc/<B-pid>/environ` because uid checks deny cross-uid reads
#
# uid 10001 / user `okto-agent` is kept around as the legacy fallback for
# the rare case where the supervisor can't assign a per-agent uid slot.
RUN useradd -u 10001 -M -N -s /bin/bash okto-agent && \
    for i in $(seq 0 99); do \
        useradd -u $((10100 + i)) -M -N -s /bin/bash okto-agent-$i; \
    done

LABEL org.opencontainers.image.source=https://github.com/georgebradford0/okto
LABEL org.opencontainers.image.description="lair — merged lair/agent binary, runs as --role lair by default"

# Operator's host dir is bind-mounted here. config.json, lair/, agents/, and
# lair-env all live under /data so they survive container restarts and stay
# visible to the host CLI for `okto logs`, `okto agents list`, etc.
ENV HOME=/data \
    OKTO_HOME=/data \
    OKTO_DATA_DIR=/data/lair \
    OKTO_AGENTS_DIR=/data/agents \
    OKTO_SKIP_SHELL_ENV=1 \
    OKTO_LAIR_BINARY=/usr/local/bin/lair \
    NOISE_PORT=8443 \
    DOCKER_CONFIG=/kaniko/.docker/ \
    SSL_CERT_DIR=/kaniko/ssl/certs

EXPOSE 8000 8443

# Default entrypoint runs the parent (lair) role. Children are spawned by lair
# itself as plain processes inside this same container — there is no bollard
# in the binary, and no second container per agent.
ENTRYPOINT ["/usr/local/bin/lair", "--role", "lair"]

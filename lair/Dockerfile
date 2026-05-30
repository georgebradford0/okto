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
# Trixie-based so the dynamically-linked libssl 3.x matches the runtime stage
# below. Mismatching the base distros across stages produces a binary that
# loads in the builder but segfaults or fails to find a SONAME at runtime.
FROM rust:1.88-slim-trixie AS builder

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
# Trixie (Debian 13, stable since mid-2025) is required for buildah ≥ 1.30 in
# the main repo — bookworm ships buildah 1.28.2 which has a hard-coded early
# `unshare(CLONE_NEWUSER)` in `containers/storage/pkg/unshare`'s init path
# that runs before flag parsing, so `--isolation chroot` is examined too late
# and Docker's default seccomp profile (which blocks the syscall for non-root)
# fails every build with "Operation not permitted". Fixed upstream in
# containers/storage#1573, first released in buildah 1.30 (~Apr 2023).
FROM debian:trixie-slim

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
        buildah \
        fuse-overlayfs \
        gcc \
        gh \
        glab \
        jq \
        libcap2-bin \
        nodejs \
        openssh-client \
        procps \
        qrencode \
        slirp4netns \
        uidmap \
        unzip \
    && rm -rf /var/lib/apt/lists/* && \
    curl -LsSf https://astral.sh/uv/install.sh | UV_INSTALL_DIR=/usr/local/bin sh

# AWS CLI v2 — official installer from AWS. No Debian package exists; the
# zip is self-contained and adds ~150 MB installed. Arch-name translation:
# Debian uses amd64/arm64, AWS uses x86_64/aarch64 in the download URL.
RUN case "$(dpkg --print-architecture)" in \
        amd64) AWS_ARCH=x86_64 ;; \
        arm64) AWS_ARCH=aarch64 ;; \
        *) echo "unsupported arch for awscli: $(dpkg --print-architecture)" >&2; exit 1 ;; \
    esac && \
    curl -fsSL "https://awscli.amazonaws.com/awscli-exe-linux-${AWS_ARCH}.zip" -o /tmp/awscli.zip && \
    unzip -q /tmp/awscli.zip -d /tmp && \
    /tmp/aws/install --install-dir /usr/local/aws-cli --bin-dir /usr/local/bin && \
    rm -rf /tmp/awscli.zip /tmp/aws

COPY --from=builder /build/target/release/lair /usr/local/bin/lair

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
# Per-agent primary group must have a gid matching the user's uid — lair's
# agent_proc.rs::uid_for_port returns `(uid, uid)` and `cmd.uid().gid()`
# sets the process's effective gid to that value. If /etc/passwd records a
# *different* primary gid (e.g. 100/users via `useradd -N`), `newgidmap`
# refuses the rootless-buildah uid-mapping setup with "Target process is
# owned by a different user" because it cross-checks pw_gid against the
# process's actual st_gid. Create a matching-gid group per user so the two
# stay in sync.
RUN groupadd -g 10001 okto-agent && \
    useradd  -u 10001 -g 10001 -m -s /bin/bash okto-agent && \
    for i in $(seq 0 99); do \
        gid=$((10100 + i)); \
        groupadd -g "$gid"            "okto-agent-$i"; \
        useradd  -u "$gid" -g "$gid" -m -s /bin/bash "okto-agent-$i"; \
    done

# Rootless container-image builds via buildah. Each non-root agent uid needs
# its own subordinate uid/gid range so buildah can map its in-container "root"
# (uid 0 of the build) onto a real uid the kernel will accept. 65536 sub-ids
# per user, tightly packed starting at 100000:
#   okto-agent          → 100000..165535
#   okto-agent-0..99    → 165536..6717439
#
# Per-user storage (vfs driver, configured below) lands at
# $HOME/.local/share/containers/storage — lair sets HOME=/data/agents/<name>/
# for each child, so storage is isolated per agent without extra mounts.
RUN { \
        printf 'okto-agent:100000:65536\n'; \
        for i in $(seq 0 99); do \
            start=$((165536 + i * 65536)); \
            printf 'okto-agent-%d:%d:65536\n' "$i" "$start"; \
        done; \
    } | tee /etc/subuid > /etc/subgid

# Container tooling configuration. vfs is the simplest storage driver — slow
# and disk-heavy but works without /dev/fuse, kernel overlay, or extra docker
# run flags. The default policy.json accepts any image source (no signature
# verification); operators that want stricter trust can override the file at
# runtime via a bind mount.
#
# storage.conf MUST set both `runroot` and `graphroot` explicitly — buildah
# refuses to start when storage.conf exists but is partially populated (it
# considers a half-spec'd config worse than no config and won't fall back to
# its built-in defaults). The rootless path uses $HOME substitution so each
# agent's storage lands under /data/agents/<name>/, preserving per-agent
# isolation without an extra mount.
RUN mkdir -p /etc/containers && \
    printf '%s\n' \
        '[storage]' \
        'driver = "vfs"' \
        'runroot = "/run/containers/storage"' \
        'graphroot = "/var/lib/containers/storage"' \
        'rootless_storage_path = "$HOME/.local/share/containers/storage"' \
        > /etc/containers/storage.conf && \
    printf 'unqualified-search-registries = ["docker.io", "ghcr.io", "quay.io"]\n' \
        > /etc/containers/registries.conf && \
    printf '{"default":[{"type":"insecureAcceptAnything"}]}\n' \
        > /etc/containers/policy.json

# Grant CAP_SYS_CHROOT as a file capability on the buildah binary so non-root
# agent uids (10100..10199) can use `--isolation chroot` for their builds.
# The kernel/seccomp/apparmor stack on most container hosts denies
# unshare(CLONE_NEWUSER), so the default `--isolation rootless` path fails for
# agents with "Operation not permitted". Without CAP_SYS_CHROOT, agents have
# no working isolation mode (oci/runc fails for similar reasons inside
# unprivileged containers). With it, both lair (root) and agents (non-root)
# can use `--isolation chroot` and the build succeeds. Modest security
# expansion — chroot alone is low-risk and the cap is scoped to the buildah
# binary, not granted to the agent process generally.
RUN setcap cap_sys_chroot+ep /usr/bin/buildah

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
    NOISE_PORT=8443

EXPOSE 8000 8443

# Default entrypoint runs the parent (lair) role. Children are spawned by lair
# itself as plain processes inside this same container — there is no bollard
# in the binary, and no second container per agent.
ENTRYPOINT ["/usr/local/bin/lair", "--role", "lair"]

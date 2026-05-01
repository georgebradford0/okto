# claudulhu — project notes for Claude

`@` is used as a path prefix to reference files in the repository (e.g. `@src/main.rs`).

## Git workflow

Do **not** create git worktrees unless explicitly asked to. Commit and push directly on the current branch.

Do **not** commit debug/diagnostic logging (`println!`, `console.log`, etc. added purely for investigation). Suggest the user add logs locally instead.

## Docker images

There is one image used for both parent and child containers:

| Image | Used by |
|-------|---------|
| `ghcr.io/georgebradford0/rulyeh` | rulyeh (parent) and all child containers |

Child Deployments use the same image with `command: ["/usr/local/bin/docker-entrypoint-server.sh"]` set in the pod spec. Each child gets its own Deployment, two PVCs (`{name}-data`, `{name}-workspace`), a ClusterIP Service (port 8000), and a NodePort Service (port 9000, assigned from range 30100–30199).

Build and push from the **repo root** (replace `X.Y.Z` with the new version). Always use `buildx` with `--platform` so both `linux/amd64` and `linux/arm64` are included in the manifest:

```sh
docker buildx build \
  --builder multiplatform \
  --platform linux/amd64,linux/arm64 \
  --push \
  -f rulyeh/Dockerfile \
  -t ghcr.io/georgebradford0/rulyeh:X.Y.Z \
  -t ghcr.io/georgebradford0/rulyeh:latest \
  .
```

## GitHub CLI

`gh` (v2.89.0) is installed and available in `$PATH`. Use it for GitHub operations (triggering workflows, creating PRs, etc.) in preference to raw `curl` API calls. `GH_TOKEN` is set in the environment so no separate `gh auth login` is needed.

---

## Architecture overview

Claudulhu is an agentic coding assistant: a server runs an AI loop against a git repo and clients (mobile, desktop) connect to it via an encrypted tunnel.

### Components

| Directory | Language | Role |
|-----------|----------|------|
| `core/` | Rust | Shared library: agentic loop, Claude API streaming, git/worktree ops, config |
| `server/` | Rust + Axum | Child container: Noise handshake → WebSocket → runs agentic loop against a single git repo |
| `rulyeh/` | Rust + Axum | Parent container: orchestrates child Kubernetes Deployments; mobile connects here first |
| `mobile/` | React Native (TS) | iOS/Android client: QR scan → native Noise tunnel → WebSocket UI |

### Transport

All client↔server communication is encrypted with **Noise_XX_25519_ChaChaPoly_SHA256**:

1. Client scans QR code → `2:<host>:<port>:<base32-pubkey>`
2. TCP connection → 3-message Noise XX handshake (mutual auth, server pubkey from QR)
3. Post-handshake: WebSocket runs over encrypted frames (2-byte big-endian length prefix)
4. JSON message types: `history`, `token`, `tool`, `question`, `done`, `error`, `ack`

Server listens on port 9000 (`NOISE_PORT`). The Curve25519 keypair is persisted in `/data`.

### rulyeh (parent container)

`rulyeh` is the parent orchestration node. The mobile client connects to it first via the QR-scanned Noise tunnel. It:

- Polls Kubernetes (every 10 s) for Deployments in the `claudulhu` namespace labelled `claudulhu.managed=1`
- Caches each child's Noise public key in `/data/pubkey_registry.json`
- Exposes `/containers` HTTP endpoint; clients poll it to get the current container list
- Accepts `start_container` commands from the client, which scale the child Deployment to 1 replica and trigger an immediate re-poll
- Runs its own agentic loop (via `core`) so the user can ask it to create/manage child containers

Image: `ghcr.io/georgebradford0/rulyeh`

#### rulyeh environment variables

| Variable | Required | Purpose |
|----------|----------|---------|
| `ANTHROPIC_API_KEY` | yes | Claude API access for parent loop |
| `GH_TOKEN` | yes | Passed to child containers on creation |
| `PUBLIC_HOST` | no | Advertised host in QR (auto-detected if unset) |
| `NOISE_PORT` | no | Listening port (default: 9000) |

### server (child container) runtime tools

The child container image (`ghcr.io/georgebradford0/rulyeh`) ships with `gh` pre-installed. When `GH_TOKEN` is set, `gh` is immediately usable inside the container without a separate `gh auth login`.

### server (child container) environment variables

| Variable | Required | Purpose |
|----------|----------|---------|
| `ANTHROPIC_API_KEY` | yes | Claude API access |
| `GIT_URL` | yes | Repo to clone and operate on |
| `GH_TOKEN` | no | GitHub token for private repos / PR creation |
| `PUBLIC_HOST` | no | Advertised host in QR (auto-detected if unset) |
| `NOISE_PORT` | no | Listening port (default: 9000) |
| `GIT_USER_NAME` / `GIT_USER_EMAIL` | no | Commit author identity |
| `RULYEH_URL` | no | HTTP URL of the parent rulyeh container (e.g. `http://rulyeh:8000`); when set, enables the `message_rulyeh` tool so the child can ask rulyeh for information or secrets |

### CI/CD workflows (all manual dispatch)

| Workflow | What it does |
|----------|-------------|
| `android.yml` | Builds AAB via fastlane, uploads to Google Play (closed/production track) |
| `ios.yml` | Builds on macOS runner, optionally uploads to TestFlight |

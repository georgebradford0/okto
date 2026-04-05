# claudulhu — project notes for Claude

## Git workflow

Do **not** create git worktrees unless explicitly asked to. Commit and push directly on the current branch.

## Docker image

The correct image name is **`ghcr.io/georgebradford0/claudulhu-server`**.

Pull:
```sh
docker pull ghcr.io/georgebradford0/claudulhu-server:latest
```

Build and push (replace `X.Y.Z` with the new version). Always use `buildx` with `--platform` so both `linux/amd64` and `linux/arm64` are included in the manifest:
```sh
docker buildx build \
  --builder multiplatform \
  --platform linux/amd64,linux/arm64 \
  --push \
  -t ghcr.io/georgebradford0/claudulhu-server:X.Y.Z \
  -t ghcr.io/georgebradford0/claudulhu-server:latest \
  .
```

**Never** use `claudulhu:latest` or any name that omits `-server`.

## GitHub CLI

`gh` (v2.89.0) is installed and available in `$PATH`. Use it for GitHub operations (triggering workflows, creating PRs, etc.) in preference to raw `curl` API calls. `GH_TOKEN` is set in the environment so no separate `gh auth login` is needed.

---

## Architecture overview

Claudulhu is an agentic coding assistant: a server runs an AI loop against a git repo and clients (mobile, desktop) connect to it via an encrypted tunnel.

### Components

| Directory | Language | Role |
|-----------|----------|------|
| `core/` | Rust | Shared library: agentic loop, Claude API streaming, git/worktree ops, config |
| `server/` | Rust + Axum | Exposed service: Noise handshake → WebSocket → runs agentic loop |
| `mobile/` | React Native (TS) | iOS/Android client: QR scan → native Noise tunnel → WebSocket UI |
| `desktop/` | Tauri + React (TS) | macOS/Linux/Windows client: same UI, connects to local or remote server |

### Transport

All client↔server communication is encrypted with **Noise_XX_25519_ChaChaPoly_SHA256**:

1. Client scans QR code → `2:<host>:<port>:<base32-pubkey>`
2. TCP connection → 3-message Noise XX handshake (mutual auth, server pubkey from QR)
3. Post-handshake: WebSocket runs over encrypted frames (2-byte big-endian length prefix)
4. JSON message types: `history`, `token`, `tool`, `question`, `done`, `error`, `ack`

Server listens on port 9000 (`NOISE_PORT`). The Curve25519 keypair is persisted in `/data`.

### Server environment variables

| Variable | Required | Purpose |
|----------|----------|---------|
| `ANTHROPIC_API_KEY` | yes | Claude API access |
| `GIT_URL` | yes | Repo to clone and operate on |
| `GH_TOKEN` | no | GitHub token for private repos / PR creation |
| `PUBLIC_HOST` | no | Advertised host in QR (auto-detected if unset) |
| `NOISE_PORT` | no | Listening port (default: 9000) |
| `GIT_USER_NAME` / `GIT_USER_EMAIL` | no | Commit author identity |

### CI/CD workflows (all manual dispatch)

| Workflow | What it does |
|----------|-------------|
| `server.yml` | Builds multi-platform Docker image and pushes to GHCR |
| `android.yml` | Builds AAB via fastlane, uploads to Google Play (closed/production track) |
| `ios.yml` | Builds on macOS runner, optionally uploads to TestFlight |

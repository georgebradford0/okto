# octo — project notes for Claude

`@` is used as a path prefix to reference files in the repository (e.g. `@src/main.rs`).

## Git workflow

Do **not** create git worktrees unless explicitly asked to. Commit and push directly on the current branch.

Do **not** commit debug/diagnostic logging (`println!`, `console.log`, etc. added purely for investigation). Suggest the user add logs locally instead.

## Docker images

One image, one binary, two roles. Both parent (lair) and child (agent) containers run the same `octo-app` binary; the image's ENTRYPOINT runs the lair role, and child Deployments override `command:` to flip the role to `agent`.

| Image | Used by |
|-------|---------|
| `ghcr.io/georgebradford0/lair` | lair (parent) and all child containers |

The image's ENTRYPOINT is `["/usr/local/bin/octo-app", "--role", "lair"]`. Child Deployments override it with `command: ["/usr/local/bin/octo-app", "--role", "agent"]` in the pod spec. There are no shell entrypoint scripts — `app/src/bootstrap.rs` does the public-IP detection, optional git clone, STARTUP_SCRIPT execution, and post-listen QR rendering directly in Rust. Each child gets its own Deployment, two PVCs (`{name}-data`, `{name}-workspace`), a ClusterIP Service (port 8000), and a NodePort Service (port 9000, assigned from range 30100–30199).

A child container is **not** required to clone a git repo. If `GIT_URL` is set the workspace is populated from that repo (with `GH_TOKEN` for HTTPS); if unset, the workspace is just `mkdir -p /workspace` and the agent runs there as a general-purpose agent. Set `AGENT_PURPOSE` (env var) to give a no-repo agent a specific mission in its system prompt.

Build and push from the **repo root** (replace `X.Y.Z` with the new version). Always use `buildx` with `--platform` so both `linux/amd64` and `linux/arm64` are included in the manifest:

```sh
docker buildx build \
  --builder multiplatform \
  --platform linux/amd64,linux/arm64 \
  --push \
  -f app/Dockerfile \
  -t ghcr.io/georgebradford0/lair:X.Y.Z \
  -t ghcr.io/georgebradford0/lair:latest \
  .
```

---

## Architecture overview

Octo is an agentic coding assistant: a server runs an AI loop against a git repo and clients (mobile, desktop) connect to it via an encrypted tunnel.

### Components

| Directory | Language | Role |
|-----------|----------|------|
| `core/` | Rust | Shared library: agentic loop, Claude API streaming, git/worktree ops, config, HTTP/WS plumbing (`core::app`) |
| `app/` | Rust + Axum | Merged binary `octo-app` with `--role lair\|agent`. `app/src/lair.rs` is the parent (orchestrates child Deployments); `app/src/agent.rs` is the child (general-purpose agent, optionally repo-scoped). Both are reachable from `app/src/main.rs`'s argparse dispatch. |
| `k8s-ops/` | Rust | Kubernetes primitives shared by lair role + CLI: Deployment / Secret / RBAC helpers, child-secrets, pod readiness waits. |
| `cli/` | Rust | `octo` CLI for managing the cluster (init, reload, pods, logs, config). |
| `mobile/` | React Native (TS) | iOS/Android client: QR scan → native Noise tunnel → WebSocket UI |

### Transport

All client↔server communication is encrypted with **Noise_XX_25519_ChaChaPoly_SHA256**:

1. Client scans QR code → `2:<host>:<port>:<base32-pubkey>`
2. TCP connection → 3-message Noise XX handshake. The QR-pinned server static key is verified during the handshake; the client's static key is captured from snow's `get_remote_static()` and logged at handshake completion (per-client allowlisting is plumbed but not yet enforced).
3. Frame format: 2-byte big-endian length prefix + ciphertext. Frames over `MAX_FRAME_SIZE` (16 KiB) are rejected; body reads timeout after 30 s; the whole handshake must complete within 10 s. Per source IP, no more than 32 concurrent Noise sessions are accepted.
4. Post-handshake: a single, persistent WebSocket runs over the encrypted frames. One WS per server (lair or child) for the entire chat session — opened on chat-screen mount, closed on unmount. `core/src/lib.rs::ChatEvent` is the wire schema; tagged JSON via serde `tag = "type"`.

Wire frames (server → client unless noted):

| `type`            | Direction | Payload                                                   |
|-------------------|-----------|-----------------------------------------------------------|
| `ready`           | s → c     | `session_id: string`, `resumed: bool`. Sent once on open. |
| `text`            | s → c     | `text: string`. Streamed live: one delta per Anthropic `content_block_delta` (text_delta) or OpenAI `choices[0].delta.content`. Mobile appends to a single message id. |
| `tool_use`        | s → c     | `tool: string`, `input: any`. Emitted once per tool call after its input JSON is fully assembled. |
| `tool_output`     | s → c     | `line: string`. One per stdout/stderr line during tool execution. |
| `tool_result`     | s → c     | `tool_use_id: string`, `output: any`. Final tool result. |
| `done`            | s → c     | `cost_usd: number`. Turn finished cleanly. |
| `interrupted`     | s → c     | `cost_usd: number`. Turn cancelled by client. |
| `interrupt_ack`   | s → c     | (no fields) Ack of an `interrupt` frame. |
| `error`           | s → c     | `message: string`. Turn failed; WS stays open. |
| `system`          | s → c     | `text: string`. Server status string for the UI. |
| `containers`      | s → c     | `containers: ContainerInfo[]`. **Lair only.** Pushed on poller change. |
| `ping`            | s → c     | `id: number`. Liveness probe every `KEEPALIVE_INTERVAL` (15 s). |
| `user_message`    | c → s     | `text: string`. Start a new agentic turn. |
| `interrupt`       | c → s     | (no fields) Cancel the in-flight turn. |
| `pong`            | c → s     | `id: number`. Reply to `ping`. After `KEEPALIVE_MAX_MISSED` (2) unacked pings the server evicts the WS. |
| `start_container` | c → s     | `id: string`. **Lair only.** Scale a child Deployment to 1. |

Mobile auto-reconnects with exponential backoff (1 s → 30 s, capped) on unintentional close; the counter resets on the next `ready`. The deprecated `GET /containers` and `POST /containers/start` HTTP endpoints have been removed (use the equivalent `/stream` events instead).

Server listens on port 9000 (`NOISE_PORT`). The Curve25519 keypair is persisted in `/data`.

### lair (parent container)

`lair` is the parent orchestration node. The mobile client connects to it first via the QR-scanned Noise tunnel. It:

- Polls Kubernetes (every 10 s) for Deployments in the `octo` namespace labelled `octo.managed=1`
- Caches each child's Noise public key in `/data/pubkey_registry.json`
- Pushes the current container list as a `containers` event over `/stream` on every poller state-change (mobile subscribes; no HTTP polling)
- Accepts `start_container` frames from the client over `/stream`, which scale the child Deployment to 1 replica and trigger an immediate re-poll
- Runs its own agentic loop (via `core`) so the user can ask it to create/manage child containers

Image: `ghcr.io/georgebradford0/lair`

#### lair environment variables

| Variable | Required | Purpose |
|----------|----------|---------|
| `ANTHROPIC_API_KEY` | yes | Claude API access for parent loop |
| `GH_TOKEN` | yes | Passed to child containers on creation |
| `PUBLIC_HOST` | no | Advertised host in QR (auto-detected if unset) |
| `NOISE_PORT` | no | Listening port (default: 9000) |

### agent (child container) environment variables

| Variable | Required | Purpose |
|----------|----------|---------|
| `ANTHROPIC_API_KEY` | yes | Claude API access |
| `GIT_URL` | no | Repo to clone into the workspace. Without it the agent runs in `/workspace` as a general-purpose agent (no git involvement). |
| `GH_TOKEN` | no | GitHub token for HTTPS clones / PR creation. Required when `GIT_URL` is HTTPS. |
| `AGENT_PURPOSE` | no | One-line mission statement baked into the system prompt when no `GIT_URL` is set |
| `WORKSPACE_DIR` | no | Workspace path (default: `/workspace`) |
| `STARTUP_SCRIPT` | no | Bash snippet run at boot after the workspace is populated |
| `STARTUP_PROMPT` | no | First user message handed to the agentic loop on boot |
| `PUBLIC_HOST` | no | Advertised host in QR (auto-detected if unset) |
| `PUBLIC_PORT` | no | Externally-reachable port (defaults to `NOISE_PORT`) |
| `NOISE_PORT` | no | Listening port (default: 9000) |
| `GIT_USER_NAME` / `GIT_USER_EMAIL` | no | Commit author identity |
| `LAIR_URL` | no | HTTP URL of the parent lair container (e.g. `http://lair:8000`); when set, enables the `message_lair` tool so the agent can ask lair for information or secrets |
| `DEPLOYMENT_NAME` | no | This pod's Deployment name. When set together with `LAIR_URL`, the agent POSTs its compiled version to lair's `/child-version` endpoint at boot so `octo reload` can show the version transition. |

### CI/CD workflows (all manual dispatch)

| Workflow | What it does |
|----------|-------------|
| `android.yml` | Builds AAB via fastlane, uploads to Google Play (closed/production track) |
| `ios.yml` | Builds on macOS runner, optionally uploads to TestFlight |

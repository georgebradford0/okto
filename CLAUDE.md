# octo — project notes for Claude

`@` is used as a path prefix to reference files in the repository (e.g. `@src/main.rs`).

## Git workflow

Do **not** create git worktrees unless explicitly asked to. Commit and push directly on the current branch.

Do **not** commit debug/diagnostic logging (`println!`, `console.log`, etc. added purely for investigation). Suggest the user add logs locally instead.

## Platform

Linux only — x86_64 and aarch64. macOS and Windows are out of scope for the runtime host. The `octo` CLI is built per-Linux-arch and published via the `cli.yml` GitHub Actions workflow; the lair runtime ships exclusively as a multi-arch Docker image (`ghcr.io/georgebradford0/octo-lair`).

Image releases run via the `lair.yml` workflow (manual dispatch): after a `lair/Cargo.toml` version bump, run `gh workflow run lair.yml --ref main`. The workflow does a `docker buildx` build for `linux/amd64,linux/arm64` on a GitHub-hosted runner and pushes to ghcr. `scripts/build-lair-image.sh` remains as a local fallback.

## Binaries & deployment

One Rust binary, `octo-lair`, two roles via `--role lair|agent`. It is built into a Docker image and never installed directly on a host.

- Operator runs `octo init`; the CLI shells out to `docker pull` + `docker run -d --name octo-lair -v ~/.octo:/data -p 8443:8443 --env-file ~/.octo/lair-env <image>`.
- The Rust code (both CLI and lair) **never imports a Docker SDK**. Every Docker interaction is a shell-out — either from `cli/src/service.rs` for lifecycle, or from the agentic loop's `bash` tool for inside-the-container diagnostics.
- When lair creates a child agent, it spawns `octo-lair --role agent` itself **inside the same container** (via `tokio::process::Command`), recording the child's pid in `/data/lair/agents.json`. There is no second container per agent.
- `lair/src/bootstrap.rs` does the public-IP detection, optional git clone, and `STARTUP_SCRIPT` execution in Rust.

Build:

```sh
# CLI — Linux host binaries, published by .github/workflows/cli.yml.
cargo build --release -p octo

# Lair image — multi-arch, pushed to ghcr by hand after each version bump.
scripts/build-lair-image.sh
```

The CLI binaries end up at `target/release/octo`; CI publishes them per-target as Release assets attached to `cli-v<version>`. The lair image is tagged `ghcr.io/georgebradford0/octo-lair:<version>` and `:latest`.

---

## Architecture overview

octo is an agentic coding assistant. A single `lair` process runs on a host machine, supervises child agent processes via a small `AgentSupervisor`, and exposes itself to a mobile client over an encrypted Noise tunnel. Mobile traffic to child agents is proxied through lair — children never get a public network surface.

### Components

| Directory | Language | Role |
|-----------|----------|------|
| `core/` | Rust | Shared library: agentic loop, Claude API streaming, git ops, config, HTTP/WS plumbing, agent registry, SSH keygen, MCP plumbing, Noise proxy. |
| `lair/` | Rust + Axum | Merged binary `octo-lair` with `--role lair|agent`. `lair/src/lair.rs` is the parent (orchestrates child processes via `lair/src/agent_proc.rs`); `lair/src/agent.rs` is the child. |
| `cli/` | Rust | `octo` CLI for managing lair on the local host (init, reload, agents, logs, mcp, config, env). |
| `mobile/` | React Native (TS) | iOS/Android client: QR scan → native Noise tunnel → single WebSocket to lair → optional proxy URL for chatting with children. |

### Filesystem layout

Everything lives under `~/.octo/` on the host, bind-mounted at `/data` inside the container:

- `~/.octo/config.json` ↔ `/data/config.json` — operator credentials (API keys, model). Read by every role via `octo_core::config_path()`. The Rust code resolves `OCTO_HOME=/data` to find this; the host CLI uses `$HOME/.octo`.
- `~/.octo/lair-env` — KEY=VALUE lines passed to `docker --env-file` on every `start_lair`. Operator-managed via `octo env`. **Stays on the host, not bind-mounted** — only consumed at container-start time.
- `~/.octo/lair-launch.json` — bookkeeping for `octo reload`: ports + last-used image reference.
- `~/.octo/lair/` ↔ `/data/lair/` — lair's per-process data dir (`OCTO_DATA_DIR`). Holds `noise_key.bin`, `agents.json`, `mcp.json`, `messages.json`, `tasks.json`, `relay_signing_key.bin`, `ssh_id_ed25519{,.pub}`, `known_hosts`. (No more `lair.pid` / `lair.log` — the container lifecycle is tracked by docker; `octo logs` shells out to `docker logs`.)
- `~/.octo/agents/<name>/` ↔ `/data/agents/<name>/` — per-agent dirs. Each has `data/` (the agent's `OCTO_DATA_DIR`) and `workspace/` (its `WORKSPACE_DIR`), plus an `agent.log` capture written by lair's supervisor.

### Agent registry

`~/.octo/lair/agents.json` is lair's single source of truth for which agents exist. Schema is `octo_core::AgentRecord`:

```
{ name, pid, port, git_url, status, binary_version, created_at, last_seen }
```

`pid` is the recorded OS pid of the last spawned `octo-lair --role agent` process. The poller checks `kill(pid, 0)` every 10s and flips `status` accordingly. On lair startup, the supervisor adopts any rows whose pid is still alive.

### Transport

Mobile ↔ lair is encrypted with **Noise_XX_25519_ChaChaPoly_SHA256**:

1. Client scans QR code → `2:<host>:<port>:<base32-pubkey>`.
2. TCP connection → 3-message Noise XX handshake.
3. Frame format: 2-byte big-endian length prefix + ciphertext. Frames over `MAX_FRAME_SIZE` (16 KiB) are rejected.
4. Post-handshake: WebSockets run over the encrypted frames.
5. **Children are *never* reached directly.** Mobile opens a WebSocket to `ws://lair/agents/<name>/stream` over the same tunnel; lair proxies frames bidirectionally to the child's loopback HTTP port via `tokio_tungstenite`.

Wire frames (see `core/src/lib.rs::ChatEvent` and `mobile/src/wire.ts`):

| `type`            | Direction | Payload |
|-------------------|-----------|---------|
| `ready`           | s → c     | `session_id`, `resumed`. Sent on each WS open. |
| `text`            | s → c     | Streamed model text deltas. |
| `tool_use` / `tool_output` / `tool_result` | s → c | Tool invocation lifecycle. |
| `done` / `interrupted` / `error` | s → c | Turn terminators. |
| `agents`          | s → c     | **Lair only.** Pushed on poller change. |
| `tasks` / `bg_complete` | s → c | Per-chat background-task registry. |
| `ping` / `pong`   | both      | Keepalive (15 s interval, 2 missed = evict). |
| `user_message`    | c → s     | Start a new agentic turn. |
| `interrupt`       | c → s     | Cancel the in-flight turn. |
| `start_agent` / `terminate_agent` | c → s | **Lair only.** Lifecycle ops. |
| `cancel_task`     | c → s     | Cancel a running background task. |

Lair listens on `NOISE_PORT` (default 8443 in prod) and forwards Noise traffic to its own HTTP server on `127.0.0.1:8000`.

### Lair management API (CLI ↔ lair on loopback)

Lair exposes a small management API on `127.0.0.1:8000` for the CLI:

- `GET    /agents` — list registry rows.
- `POST   /agents` — `{name?, git_url?, port?, startup_script?, startup_prompt?}` — spawn a new child.
- `POST   /agents/:name/start` / `stop` — supervisor control.
- `DELETE /agents/:name` — terminate + remove data dir.
- `GET    /agents/:name/logs` — last 1 MB of the child's `agent.log`.
- `GET    /agents/:name/stream` — WebSocket proxy (mobile end).
- `GET    /agents/:name/history`, `POST /agents/:name/interrupt`, `POST /agents/:name/clear`, `GET /agents/:name/branches` — HTTP proxies of the child's same-name endpoints.

### lair credentials (`~/.octo/config.json`)

Lair reads its API keys and provider settings from `~/.octo/config.json` (mapped to `/data/config.json` in-container). Lair re-reads it on every model call, so rotation is live without restarting the container. Children inherit credentials via env at spawn time.

`GH_TOKEN` and any other operator-supplied env vars live in `~/.octo/lair-env` (operator-managed via `octo env set KEY=VAL`). The file is passed to `docker run --env-file`, so anything in it reaches the lair container's process env — and is then inherited by every child agent process lair spawns (tokio's `Command` default).

### lair runtime env

Baked into the image (see `lair/Dockerfile`):

| Variable | Purpose |
|----------|---------|
| `HOME` / `OCTO_HOME` | `/data` (so config + ssh keys resolve to bind-mounted host paths) |
| `OCTO_DATA_DIR` | `/data/lair` |
| `OCTO_AGENTS_DIR` | `/data/agents` |
| `OCTO_LAIR_BINARY` | `/usr/local/bin/octo-lair` (used to spawn children) |
| `OCTO_SKIP_SHELL_ENV` | Always 1; suppresses the login-shell env sourcing |

Set at `docker run` time by `cli/src/service.rs`:

| Variable | Purpose |
|----------|---------|
| `PUBLIC_PORT` | Noise port the QR code advertises (matches the host-side `-p` mapping) |

`NOISE_PORT` inside the container is hardcoded to the `EXPOSE`d 8443; the host-side port mapping is what the operator controls via `--noise-port`.

### agent (child) env (set by lair on spawn)

| Variable | Purpose |
|----------|---------|
| `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `MODEL` / `OPENAI_API_URL` | Provider credentials |
| `AGENT_PORT` | Loopback HTTP port (30100–30199) |
| `OCTO_DATA_DIR` | `~/.octo/agents/<name>/data` |
| `WORKSPACE_DIR` | `~/.octo/agents/<name>/workspace` |
| `GIT_URL` | Optional repo to clone (HTTPS needs `GH_TOKEN`) |
| `AGENT_PURPOSE` / `STARTUP_SCRIPT` / `STARTUP_PROMPT` | Optional bootstrap knobs |

### CI/CD workflows (all manual dispatch)

| Workflow | What it does |
|----------|--------------|
| `cli.yml` | Builds the `octo` CLI per-target and uploads as Release assets. |
| `lair.yml` | Multi-arch `docker buildx` of `lair/Dockerfile`, pushes to `ghcr.io/<owner>/octo-lair:<version>` + `:latest`. |
| `android.yml` | Builds AAB via fastlane, uploads to Google Play. |
| `ios.yml` | Builds on macOS runner, optionally uploads to TestFlight. |

`scripts/build-lair-image.sh` is a local fallback for `lair.yml` when CI is unavailable.
